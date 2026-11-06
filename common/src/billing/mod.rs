//! Fork Crateús/CE — energy bookkeeping for the electricity bill.
//!
//! * `energy_hourly` keeps the measured energy per local hour and per source forever
//!   (it is never touched by the purge of raw sensor data).
//! * `tariff` downloads the regulated tariff (TE + TUSD) and the monthly "bandeira" from the
//!   ANEEL open-data API and caches them in the database.
//! * `report` turns both into per-billing-month costs and a compact Markdown export.
pub mod report;
pub mod tariff;

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{ComputedSensorData, MetricSample, MonitorPowerState, MonitorReading};

/// Source name used for the external monitor in `energy_hourly`.
pub const SOURCE_MONITOR: &str = "monitor";
/// Source name of the whole-PC measured total (same name as the sensor table).
pub const SOURCE_TOTAL: &str = "total_data";

/// Longest sample duration accounted as "time on" (protects against sleep/hibernate gaps).
pub const MAX_ACCOUNTED_SAMPLE: Duration = Duration::from_secs(5);

/// Creates the billing tables (idempotent, safe to call from the UI and the collector).
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS energy_hourly (
            hour      TEXT    NOT NULL,
            source    TEXT    NOT NULL,
            energy_uj INTEGER NOT NULL DEFAULT 0,
            active_ms INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (hour, source)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS tariff_periods (
            distributor TEXT NOT NULL,
            start_date  TEXT NOT NULL,
            end_date    TEXT NOT NULL,
            tusd_mwh    REAL NOT NULL,
            te_mwh      REAL NOT NULL,
            resolution  TEXT,
            origin      TEXT NOT NULL DEFAULT 'aneel',
            PRIMARY KEY (distributor, start_date)
        );

        CREATE TABLE IF NOT EXISTS tariff_flags (
            month   TEXT PRIMARY KEY,
            flag    TEXT NOT NULL,
            add_mwh REAL NOT NULL
        );

        CREATE TABLE IF NOT EXISTS billing_kv (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS monitor_status (
            id            INTEGER PRIMARY KEY CHECK (id = 1),
            updated_ms    INTEGER NOT NULL,
            name          TEXT NOT NULL,
            state         TEXT NOT NULL,
            brightness    INTEGER,
            watts         REAL NOT NULL,
            ddc_ok        INTEGER NOT NULL DEFAULT 0,
            virtual_names TEXT NOT NULL DEFAULT ''
        );",
    )
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// User-adjustable parameters of the bill estimate (stored in `billing_kv`).
#[derive(Debug, Clone, PartialEq)]
pub struct BillingSettings {
    /// ANEEL `SigAgente` of the distributor (Crateús/CE is served by ENEL CE).
    pub distributor: String,
    pub region_label: String,
    /// ICMS (%), Ceará charges 20% on electricity since 2024 (Lei 18.305/2023).
    pub icms_pct: f64,
    /// PIS + COFINS effective rate (%), varies monthly (see your bill).
    pub pis_cofins_pct: f64,
    /// Day of the month the meter reading cycle starts (1 = calendar month).
    pub billing_day: u32,
    /// Constant extra wall power while the PC is on (motherboard, fans, peripherals) not seen by sensors.
    pub extra_watts: f64,
    pub monitor_enabled: bool,
    /// Monitor model: watts at brightness 0 and 100, and in standby/off.
    pub monitor_w_min: f64,
    pub monitor_w_max: f64,
    pub monitor_w_standby: f64,
    /// Brightness assumed when DDC/CI cannot be read.
    pub monitor_default_brightness: f64,
    /// Only count monitors whose hardware id / name contains this text (empty = every physical monitor).
    pub monitor_match: String,
    /// Use the ANEEL tariff for the dashboard bill (otherwise the upstream preset/custom price).
    pub use_auto_tariff: bool,
}

impl Default for BillingSettings {
    fn default() -> Self {
        Self {
            distributor: "ENEL CE".to_string(),
            region_label: "Crateús - CE".to_string(),
            icms_pct: 20.0,
            pis_cofins_pct: 5.7,
            billing_day: 1,
            extra_watts: 0.0,
            monitor_enabled: true,
            // AOC 27G4: manual says 31 W typical (default brightness 90) and <=46 W at
            // brightness/contrast 100 incl. USB/audio; EU label 24 W. Linear model 8..34 W
            // reproduces both; 0.3 W standby/off.
            monitor_w_min: 8.0,
            monitor_w_max: 34.0,
            monitor_w_standby: 0.3,
            monitor_default_brightness: 90.0,
            monitor_match: String::new(),
            use_auto_tariff: true,
        }
    }
}

impl BillingSettings {
    /// Total tax rate "por dentro" (ICMS + PIS/COFINS), clamped to a sane range.
    pub fn tax_rate(&self) -> f64 {
        ((self.icms_pct + self.pis_cofins_pct) / 100.0).clamp(0.0, 0.9)
    }

    /// Estimated wall power of one monitor for a given brightness (0-100).
    pub fn monitor_watts(&self, brightness: f64) -> f64 {
        let b = brightness.clamp(0.0, 100.0) / 100.0;
        (self.monitor_w_min + (self.monitor_w_max - self.monitor_w_min) * b).max(0.0)
    }
}

pub fn kv_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM billing_kv WHERE key = ?1", params![key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
}

pub fn kv_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO billing_kv (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn kv_f64(conn: &Connection, key: &str, default: f64) -> f64 {
    kv_get(conn, key)
        .and_then(|v| v.trim().replace(',', ".").parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

fn kv_bool(conn: &Connection, key: &str, default: bool) -> bool {
    kv_get(conn, key).map(|v| v == "1" || v == "true").unwrap_or(default)
}

pub fn load_settings(conn: &Connection) -> BillingSettings {
    let d = BillingSettings::default();
    BillingSettings {
        distributor: kv_get(conn, "distributor")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(d.distributor),
        region_label: kv_get(conn, "region_label")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(d.region_label),
        icms_pct: kv_f64(conn, "icms_pct", d.icms_pct).clamp(0.0, 50.0),
        pis_cofins_pct: kv_f64(conn, "pis_cofins_pct", d.pis_cofins_pct).clamp(0.0, 30.0),
        billing_day: (kv_f64(conn, "billing_day", d.billing_day as f64) as u32).clamp(1, 28),
        extra_watts: kv_f64(conn, "extra_watts", d.extra_watts).clamp(0.0, 1000.0),
        monitor_enabled: kv_bool(conn, "monitor_enabled", d.monitor_enabled),
        monitor_w_min: kv_f64(conn, "monitor_w_min", d.monitor_w_min).clamp(0.0, 500.0),
        monitor_w_max: kv_f64(conn, "monitor_w_max", d.monitor_w_max).clamp(0.0, 500.0),
        monitor_w_standby: kv_f64(conn, "monitor_w_standby", d.monitor_w_standby).clamp(0.0, 50.0),
        monitor_default_brightness: kv_f64(conn, "monitor_default_brightness", d.monitor_default_brightness)
            .clamp(0.0, 100.0),
        monitor_match: kv_get(conn, "monitor_match").unwrap_or(d.monitor_match),
        use_auto_tariff: kv_bool(conn, "use_auto_tariff", d.use_auto_tariff),
    }
}

pub fn save_settings(conn: &Connection, s: &BillingSettings) -> rusqlite::Result<()> {
    let b = |v: bool| if v { "1" } else { "0" }.to_string();
    let pairs: [(&str, String); 13] = [
        ("distributor", s.distributor.trim().to_string()),
        ("region_label", s.region_label.trim().to_string()),
        ("icms_pct", s.icms_pct.to_string()),
        ("pis_cofins_pct", s.pis_cofins_pct.to_string()),
        ("billing_day", s.billing_day.to_string()),
        ("extra_watts", s.extra_watts.to_string()),
        ("monitor_enabled", b(s.monitor_enabled)),
        ("monitor_w_min", s.monitor_w_min.to_string()),
        ("monitor_w_max", s.monitor_w_max.to_string()),
        ("monitor_w_standby", s.monitor_w_standby.to_string()),
        ("monitor_default_brightness", s.monitor_default_brightness.to_string()),
        ("monitor_match", s.monitor_match.trim().to_string()),
        ("use_auto_tariff", b(s.use_auto_tariff)),
    ];
    for (k, v) in pairs {
        kv_set(conn, k, &v)?;
    }
    Ok(())
}

/// PSU efficiency chosen in the upstream settings (1.0 = none / measured values are wall values).
pub fn load_psu_efficiency(conn: &Connection) -> f64 {
    conn.query_row("SELECT psu_efficiency FROM ui_settings WHERE id = 1", [], |row| {
        row.get::<_, Option<f64>>(0)
    })
    .optional()
    .ok()
    .flatten()
    .flatten()
    .filter(|v| v.is_finite() && *v > 0.3 && *v <= 1.0)
    .unwrap_or(1.0)
}

// ---------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------

/// Local hour key `YYYY-MM-DD HH` for a timestamp.
pub fn hour_key(ts: SystemTime) -> String {
    DateTime::<Local>::from(ts).format("%Y-%m-%d %H").to_string()
}

const UPSERT_HOURLY: &str = "INSERT INTO energy_hourly (hour, source, energy_uj, active_ms) VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT(hour, source) DO UPDATE SET \
       energy_uj = energy_uj + excluded.energy_uj, \
       active_ms = active_ms + excluded.active_ms";

/// Adds one collector sample to `energy_hourly` (call inside the sample transaction).
pub fn record_sample(conn: &Connection, sample: &MetricSample) -> rusqlite::Result<()> {
    let hour = hour_key(sample.timestamp);
    let active_ms = sample.duration.min(MAX_ACCOUNTED_SAMPLE).as_millis() as i64;
    let mut stmt = conn.prepare_cached(UPSERT_HOURLY)?;

    // Several GPUs share one source name: count active time once per source.
    let mut seen: Vec<&'static str> = Vec::new();
    for data in sample.data() {
        if matches!(data, ComputedSensorData::Process(_)) {
            continue;
        }
        if let Some(energy) = data.total_energy() {
            let source = data.table_name();
            let ms = if seen.contains(&source) { 0 } else { active_ms };
            seen.push(source);
            stmt.execute(params![hour, source, energy.as_u64() as i64, ms])?;
        }
    }

    if let Some(monitor) = &sample.monitor {
        let on_ms = if monitor.state.is_on() { active_ms } else { 0 };
        stmt.execute(params![hour, SOURCE_MONITOR, monitor.energy.as_u64() as i64, on_ms])?;
        write_monitor_status(conn, monitor, sample.timestamp_ms())?;
    }
    Ok(())
}

fn write_monitor_status(conn: &Connection, m: &MonitorReading, now_ms: i64) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO monitor_status (id, updated_ms, name, state, brightness, watts, ddc_ok, virtual_names) \
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO UPDATE SET updated_ms = ?1, name = ?2, state = ?3, brightness = ?4, \
           watts = ?5, ddc_ok = ?6, virtual_names = ?7",
    )?
    .execute(params![
        now_ms,
        m.name,
        m.state.code(),
        m.brightness,
        m.watts,
        m.ddc_ok as i64,
        m.virtual_displays.join(", ")
    ])?;
    Ok(())
}

/// Latest monitor status written by the collector.
#[derive(Debug, Clone)]
pub struct MonitorStatus {
    pub updated_ms: i64,
    pub name: String,
    pub state: MonitorPowerState,
    pub brightness: Option<u32>,
    pub watts: f64,
    pub ddc_ok: bool,
    pub virtual_names: String,
}

pub fn read_monitor_status(conn: &Connection) -> Option<MonitorStatus> {
    conn.query_row(
        "SELECT updated_ms, name, state, brightness, watts, ddc_ok, virtual_names FROM monitor_status WHERE id = 1",
        [],
        |row| {
            Ok(MonitorStatus {
                updated_ms: row.get(0)?,
                name: row.get(1)?,
                state: MonitorPowerState::from_code(&row.get::<_, String>(2)?),
                brightness: row.get::<_, Option<i64>>(3)?.map(|b| b.clamp(0, 100) as u32),
                watts: row.get(4)?,
                ddc_ok: row.get::<_, i64>(5)? != 0,
                virtual_names: row.get(6)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// One-time import of the history recorded before this fork (hourly averages + recent raw rows of
/// `total_data`), so the first months are not empty. Runs in a single transaction, at most once.
pub fn backfill_from_history(conn: &mut Connection) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    if kv_get(&tx, "backfill_v1").is_some() {
        return Ok(0);
    }
    let has_total: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='total_data')",
        [],
        |row| row.get(0),
    )?;
    let mut inserted = 0;
    if has_total {
        // Hourly rows (duration >= 1 h) win; raw rows are only used for hours without an hourly row.
        inserted = tx.execute(
            "INSERT INTO energy_hourly (hour, source, energy_uj, active_ms)
             SELECT h, 'total_data', SUM(e), SUM(MIN(d, 3600000)) FROM (
                 SELECT strftime('%Y-%m-%d %H', timestamp / 1000, 'unixepoch', 'localtime') AS h,
                        total_energy_uj AS e,
                        duration_ms AS d,
                        duration_ms >= 3600000 AS is_hourly
                 FROM total_data
             ) t
             WHERE t.is_hourly = 1 OR t.h NOT IN (
                 SELECT strftime('%Y-%m-%d %H', timestamp / 1000, 'unixepoch', 'localtime')
                 FROM total_data WHERE duration_ms >= 3600000
             )
             GROUP BY h
             ON CONFLICT(hour, source) DO UPDATE SET
                 energy_uj = energy_uj + excluded.energy_uj,
                 active_ms = active_ms + excluded.active_ms",
            [],
        )?;
    }
    kv_set(&tx, "backfill_v1", &Local::now().to_rfc3339())?;
    tx.commit()?;
    Ok(inserted)
}

// ---------------------------------------------------------------------------------------------
// Daily usage + CSV backup
// ---------------------------------------------------------------------------------------------

/// Energy of one local day, all values in Wh / hours.
#[derive(Debug, Clone, Default)]
pub struct DayUsage {
    /// PC energy as measured by the sensors (DC side, before PSU losses).
    pub pc_measured_wh: f64,
    pub pc_on_hours: f64,
    pub monitor_wh: f64,
    pub monitor_on_hours: f64,
    /// Per component (cpu_data, gpu_data, ...), measured Wh.
    pub components: HashMap<String, f64>,
}

/// Loads per-day usage between two dates (inclusive), keyed by `YYYY-MM-DD`.
pub fn load_daily_usage(
    conn: &Connection,
    first_day: &str,
    last_day: &str,
) -> rusqlite::Result<Vec<(String, DayUsage)>> {
    let mut stmt = conn.prepare(
        "SELECT substr(hour, 1, 10) AS day, source, SUM(energy_uj), SUM(active_ms) \
         FROM energy_hourly \
         WHERE substr(hour, 1, 10) >= ?1 AND substr(hour, 1, 10) <= ?2 \
         GROUP BY day, source ORDER BY day",
    )?;
    let rows = stmt.query_map(params![first_day, last_day], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut out: Vec<(String, DayUsage)> = Vec::new();
    for row in rows {
        let (day, source, uj, ms) = row?;
        if out.last().map_or(true, |(d, _)| *d != day) {
            out.push((day.clone(), DayUsage::default()));
        }
        let usage = &mut out.last_mut().expect("just pushed").1;
        let wh = uj.max(0) as f64 / 3_600_000_000.0;
        let hours = ms.max(0) as f64 / 3_600_000.0;
        match source.as_str() {
            SOURCE_TOTAL => {
                usage.pc_measured_wh += wh;
                usage.pc_on_hours += hours;
            }
            SOURCE_MONITOR => {
                usage.monitor_wh += wh;
                usage.monitor_on_hours += hours;
            }
            other => {
                *usage.components.entry(other.to_string()).or_insert(0.0) += wh;
            }
        }
    }
    Ok(out)
}

/// First day with recorded energy, if any.
pub fn first_recorded_day(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT MIN(substr(hour, 1, 10)) FROM energy_hourly", [], |row| {
        row.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
}

/// Writes `relatorios/historico-diario.csv` (one line per day) atomically: an independent,
/// human-readable backup of everything the bill estimate is based on.
pub fn export_daily_csv(conn: &Connection, dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("historico-diario.csv");
    let tmp = dir.join("historico-diario.csv.tmp");
    let days = load_daily_usage(conn, "0000-00-00", "9999-99-99").map_err(std::io::Error::other)?;
    let mut f = fs::File::create(&tmp)?;
    writeln!(
        f,
        "dia;pc_kwh_medido;pc_horas_ligado;monitor_kwh;monitor_horas_ligado;cpu_kwh;gpu_kwh;ram_kwh;disco_kwh;rede_kwh"
    )?;
    for (day, u) in days {
        let c = |k: &str| u.components.get(k).copied().unwrap_or(0.0) / 1000.0;
        writeln!(
            f,
            "{day};{:.5};{:.3};{:.5};{:.3};{:.5};{:.5};{:.5};{:.5};{:.5}",
            u.pc_measured_wh / 1000.0,
            u.pc_on_hours,
            u.monitor_wh / 1000.0,
            u.monitor_on_hours,
            c("cpu_data"),
            c("gpu_data"),
            c("ram_data"),
            c("disk_data"),
            c("network_data"),
        )?;
    }
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::types::{EnergyUj, TotalData};

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn settings_roundtrip_and_defaults() {
        let conn = mem();
        let d = load_settings(&conn);
        assert_eq!(d, BillingSettings::default());
        let mut s = d.clone();
        s.icms_pct = 18.0;
        s.billing_day = 15;
        s.monitor_enabled = false;
        save_settings(&conn, &s).unwrap();
        assert_eq!(load_settings(&conn), s);
    }

    #[test]
    fn monitor_model_is_linear() {
        let s = BillingSettings::default();
        assert!((s.monitor_watts(0.0) - 8.0).abs() < 1e-9);
        assert!((s.monitor_watts(100.0) - 34.0).abs() < 1e-9);
        assert!((s.monitor_watts(90.0) - 31.4).abs() < 1e-9);
    }

    #[test]
    fn samples_accumulate_per_hour() {
        let conn = mem();
        let ts = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for _ in 0..3 {
            let sample = MetricSample::new(
                ts,
                Duration::from_secs(1),
                vec![ComputedSensorData::Total(TotalData {
                    total_energy: EnergyUj::from_joules(100.0),
                })],
            )
            .with_monitor(Some(MonitorReading {
                name: "27G4".into(),
                state: MonitorPowerState::On,
                brightness: Some(80),
                watts: 28.8,
                energy: EnergyUj::from_joules(28.8),
                virtual_displays: vec![],
                ddc_ok: true,
            }));
            record_sample(&conn, &sample).unwrap();
        }
        let day = DateTime::<Local>::from(ts).format("%Y-%m-%d").to_string();
        let days = load_daily_usage(&conn, &day, &day).unwrap();
        assert_eq!(days.len(), 1);
        let u = &days[0].1;
        assert!((u.pc_measured_wh - 300.0 / 3600.0).abs() < 1e-6);
        assert!((u.monitor_wh - 86.4 / 3600.0).abs() < 1e-6);
        assert!((u.pc_on_hours - 3.0 / 3600.0).abs() < 1e-9);
        let status = read_monitor_status(&conn).unwrap();
        assert_eq!(status.state, MonitorPowerState::On);
        assert_eq!(status.brightness, Some(80));
    }

    #[test]
    fn long_gaps_do_not_count_as_time_on() {
        let conn = mem();
        let sample = MetricSample::new(
            SystemTime::now(),
            Duration::from_secs(8 * 3600),
            vec![ComputedSensorData::Total(TotalData {
                total_energy: EnergyUj::from_joules(1.0),
            })],
        );
        record_sample(&conn, &sample).unwrap();
        let ms: i64 = conn
            .query_row("SELECT SUM(active_ms) FROM energy_hourly", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, 5000);
    }

    #[test]
    fn backfill_prefers_hourly_rows_and_runs_once() {
        let mut conn = mem();
        conn.execute_batch(
            "CREATE TABLE total_data (timestamp INTEGER, duration_ms INTEGER, total_energy_uj INTEGER);",
        )
        .unwrap();
        let hour_start: i64 = 1_800_000_000_000 - (1_800_000_000_000 % 3_600_000);
        // hourly aggregate of 100 Wh
        conn.execute(
            "INSERT INTO total_data VALUES (?1, 3600000, ?2)",
            params![hour_start, 360_000_000_000i64],
        )
        .unwrap();
        // stale raw row inside the same hour must be ignored
        conn.execute(
            "INSERT INTO total_data VALUES (?1, 1000, 999999)",
            params![hour_start + 10_000],
        )
        .unwrap();
        // raw rows in the next hour are used
        conn.execute(
            "INSERT INTO total_data VALUES (?1, 1000, 3600000000)",
            params![hour_start + 3_600_000 + 5_000],
        )
        .unwrap();
        assert_eq!(backfill_from_history(&mut conn).unwrap(), 2);
        assert_eq!(backfill_from_history(&mut conn).unwrap(), 0);
        let total: i64 = conn
            .query_row("SELECT SUM(energy_uj) FROM energy_hourly", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 360_000_000_000 + 3_600_000_000);
    }
}
