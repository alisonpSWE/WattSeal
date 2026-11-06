use std::{
    thread,
    time::{Duration, Instant},
};

use common::{
    MetricSample, clog,
    database::{Database, purge::averaging_and_purging_data},
    types::HardwareInfo,
};

/// Connector that writes collected sensor metrics into the local SQLite database.
pub struct DatabaseConnector {
    database: Database,
    last_purge: Instant,
}

impl DatabaseConnector {
    /// Initializes the database connection, creates default tables, saves hardware info,
    /// and triggers initial data averaging and purging in the background.
    pub fn new(hardware: &HardwareInfo) -> Result<Self, String> {
        clog!("\n========== SETTING UP DATABASE ==========");
        let mut db = Database::new().map_err(|e| format!("Failed to create database: {e}"))?;
        db.create_tables_if_not_exists(hardware)
            .map_err(|e| format!("Failed to create database tables: {e}"))?;
        clog!("✓ Database tables initialized");

        match db.insert_hardware_info(hardware) {
            Ok(_) => clog!("✓ Hardware info saved to database"),
            Err(e) => clog!("✗ Failed to save hardware info to database: {e}"),
        }

        // Fork: import the pre-fork history once, then keep the tariff cache fresh.
        match common::billing::backfill_from_history(db.connection_mut()) {
            Ok(0) => {}
            Ok(n) => clog!("✓ Imported {n} hour(s) of previous history into the bill history"),
            Err(e) => clog!("✗ Failed to import previous history: {e}"),
        }
        spawn_billing_maintenance();

        let mut connector = Self {
            database: db,
            last_purge: Instant::now(),
        };
        connector.purge_and_average();
        Ok(connector)
    }

    /// Spawns a background thread to aggregate raw data into hourly averages and purge expired entries.
    pub fn purge_and_average(&mut self) {
        thread::spawn(|| {
            if let Ok(mut db) = Database::new() {
                let _ = averaging_and_purging_data(&mut db, 24, 24);
            }
        });
        self.last_purge = Instant::now();
    }

    /// Inserts a newly collected metric sample into the database.
    pub fn handle_sample(&mut self, sample: &MetricSample) -> Result<(), String> {
        if self.last_purge.elapsed() > Duration::from_secs(24 * 3600) {
            self.purge_and_average();
        }

        #[cfg(debug_assertions)]
        let start = Instant::now();

        let result = self.database.insert_sample_and_update_energy(sample);

        #[cfg(debug_assertions)]
        {
            let duration = start.elapsed();
            match &result {
                Ok(_) => println!("✓ Event data saved to database (took {:.2?})", duration),
                Err(e) => eprintln!("✗ Failed to save event data: {:?}", e),
            }
        }

        result.map_err(|e| format!("Failed to save event data to database: {e}"))
    }
}

/// Background thread: refreshes the ANEEL tariff cache every few hours and rewrites the daily CSV
/// backup (`relatorios/historico-diario.csv`) every 30 min. Uses its own connection.
fn spawn_billing_maintenance() {
    let spawned = thread::Builder::new()
        .name("billing-maintenance".into())
        .spawn(|| {
            let mut last_tariff: Option<Instant> = None;
            loop {
                if let Ok(mut db) = Database::new() {
                    let settings = common::billing::load_settings(db.connection());
                    let _ = common::billing::tariff::seed_defaults(db.connection(), &settings.distributor);
                    let due = last_tariff.map_or(true, |t| t.elapsed() >= common::billing::tariff::REFRESH_INTERVAL);
                    if due {
                        let outcome = common::billing::tariff::refresh(db.connection_mut(), &settings.distributor);
                        if outcome.errors.is_empty() {
                            clog!(
                                "✓ ANEEL tariff cache refreshed ({} period(s), {} flag month(s))",
                                outcome.periods,
                                outcome.flags
                            );
                            last_tariff = Some(Instant::now());
                        } else {
                            clog!("⚠ ANEEL tariff refresh: {}", outcome.errors.join(" | "));
                            // Retry sooner after a failure (30 min) instead of waiting the full interval.
                            last_tariff = Some(
                                Instant::now()
                                    .checked_sub(common::billing::tariff::REFRESH_INTERVAL - Duration::from_secs(1800))
                                    .unwrap_or_else(Instant::now),
                            );
                        }
                    }
                    if let Err(e) =
                        common::billing::export_daily_csv(db.connection(), &common::paths::reports_dir())
                    {
                        clog!("✗ Failed to write daily CSV backup: {e}");
                    }
                }
                thread::sleep(Duration::from_secs(1800));
            }
        });
    if let Err(e) = spawned {
        clog!("✗ Failed to start billing maintenance thread: {e}");
    }
}
