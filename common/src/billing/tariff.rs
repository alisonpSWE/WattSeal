//! Regulated residential tariff from the ANEEL open-data portal (CKAN datastore API).
//!
//! * Tariffs: dataset "Tarifas de aplicação das distribuidoras de energia elétrica", filtered to
//!   B1 / Residencial / Residencial / Convencional / "Tarifa de Aplicação" (R$/MWh, TE + TUSD).
//! * Bandeiras: dataset "Bandeiras Tarifárias — Acionamento" (flag and R$/MWh per month).
//!
//! Everything is cached in SQLite, so the estimate keeps working offline; a few known periods
//! for ENEL CE are seeded so a fresh install without internet is still correct.
use std::{collections::BTreeMap, time::Duration};

use chrono::{Datelike, Local, NaiveDate};
use rusqlite::{Connection, params};
use serde_json::Value;

use super::{BillingSettings, kv_get, kv_set};

pub const ANEEL_API: &str = "https://dadosabertos.aneel.gov.br/api/3/action/datastore_search";
pub const RESOURCE_TARIFFS: &str = "fcf2906c-7c32-4b9b-a637-054e7a5234f4";
pub const RESOURCE_FLAGS: &str = "0591b8f6-fe54-437b-b72b-1aa2efd46e42";

/// How often the collector refreshes the cache.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Known ENEL CE B1 residential periods (ANEEL "Tarifa de Aplicação", R$/MWh), used only when the
/// cache is empty. Source: ANEEL datastore, fetched 2026-11-05.
const SEED_ENEL_CE: &[(&str, &str, f64, f64, &str)] = &[
    ("2024-04-22", "2025-04-21", 449.29, 272.91, "REH nº 3.319/2024"),
    ("2025-04-22", "2025-12-31", 464.88, 245.19, "REH nº 3.445/2025"),
    ("2026-01-01", "2026-04-21", 464.88, 245.19, "REH nº 3.445/2025"),
    ("2026-04-22", "2026-08-25", 488.78, 260.99, "REH nº 3.574/2026"),
    ("2026-08-26", "2027-04-21", 488.49, 213.26, "REH nº 3.324/2026"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct TariffPeriod {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub tusd_mwh: f64,
    pub te_mwh: f64,
    pub resolution: String,
}

impl TariffPeriod {
    /// TE + TUSD without taxes, R$/kWh.
    pub fn base_kwh(&self) -> f64 {
        (self.tusd_mwh + self.te_mwh) / 1000.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlagMonth {
    /// `YYYY-MM`
    pub month: String,
    pub flag: String,
    pub add_mwh: f64,
}

/// Parses Brazilian formatted numbers like `488,49`, `,00`, `1.234,5`.
pub fn parse_br_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let normalized = if t.contains(',') {
        t.replace('.', "").replace(',', ".")
    } else {
        t.to_string()
    };
    let normalized = if normalized.starts_with('.') {
        format!("0{normalized}")
    } else if normalized.starts_with("-.") {
        normalized.replacen("-.", "-0.", 1)
    } else {
        normalized
    };
    normalized.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn field<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or("")
}

fn is_not_applicable(s: &str) -> bool {
    let l = s.to_lowercase();
    l.starts_with("n") && l.contains("aplica")
}

/// Extracts residential B1 conventional application tariffs from datastore records.
pub fn parse_tariff_records(records: &[Value]) -> Vec<TariffPeriod> {
    let mut by_start: BTreeMap<NaiveDate, TariffPeriod> = BTreeMap::new();
    for r in records {
        if !field(r, "DscBaseTarifaria").to_lowercase().starts_with("tarifa de aplica")
            || field(r, "DscModalidadeTarifaria") != "Convencional"
            || field(r, "DscSubGrupo") != "B1"
            || field(r, "DscClasse") != "Residencial"
            || field(r, "DscSubClasse") != "Residencial"
            || !is_not_applicable(field(r, "DscDetalhe"))
            || !field(r, "DscUnidadeTerciaria").eq_ignore_ascii_case("MWh")
        {
            continue;
        }
        let posto = field(r, "NomPostoTarifario");
        if !posto.is_empty() && !is_not_applicable(posto) {
            continue;
        }
        let acessante = field(r, "SigAgenteAcessante");
        if !acessante.is_empty() && !is_not_applicable(acessante) {
            continue;
        }
        let (Ok(start), Ok(end)) = (
            NaiveDate::parse_from_str(field(r, "DatInicioVigencia"), "%Y-%m-%d"),
            NaiveDate::parse_from_str(field(r, "DatFimVigencia"), "%Y-%m-%d"),
        ) else {
            continue;
        };
        let (Some(tusd), Some(te)) = (
            parse_br_number(field(r, "VlrTUSD")),
            parse_br_number(field(r, "VlrTE")),
        ) else {
            continue;
        };
        if tusd <= 0.0 || te <= 0.0 {
            continue;
        }
        by_start.insert(
            start,
            TariffPeriod {
                start,
                end,
                tusd_mwh: tusd,
                te_mwh: te,
                resolution: field(r, "DscREH").trim().to_string(),
            },
        );
    }
    by_start.into_values().collect()
}

pub fn parse_flag_records(records: &[Value]) -> Vec<FlagMonth> {
    let mut by_month: BTreeMap<String, FlagMonth> = BTreeMap::new();
    for r in records {
        let competencia = field(r, "DatCompetencia");
        if competencia.len() < 7 {
            continue;
        }
        let month = competencia[..7].to_string();
        let flag = field(r, "NomBandeiraAcionada").trim().to_string();
        let Some(add) = parse_br_number(field(r, "VlrAdicionalBandeira")) else {
            continue;
        };
        if flag.is_empty() {
            continue;
        }
        by_month.insert(
            month.clone(),
            FlagMonth {
                month,
                flag,
                add_mwh: add.max(0.0),
            },
        );
    }
    by_month.into_values().collect()
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(45)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn datastore(agent: &ureq::Agent, query: &[(&str, &str)]) -> Result<Vec<Value>, String> {
    let mut request = agent
        .get(ANEEL_API)
        .header("User-Agent", "WattSeal-fork (consumo residencial)");
    for (k, v) in query {
        request = request.query(*k, *v);
    }
    let mut response = request.call().map_err(|e| format!("ANEEL: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("ANEEL: {e}"))?;
    let json: Value = serde_json::from_str(&body).map_err(|e| format!("ANEEL JSON: {e}"))?;
    if json.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("ANEEL: resposta sem sucesso".to_string());
    }
    Ok(json
        .pointer("/result/records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

pub fn fetch_tariffs(distributor: &str) -> Result<Vec<TariffPeriod>, String> {
    let filters = serde_json::json!({
        "SigAgente": distributor,
        "DscSubGrupo": "B1",
        "DscClasse": "Residencial",
        "DscSubClasse": "Residencial",
        "DscModalidadeTarifaria": "Convencional",
    })
    .to_string();
    let records = datastore(
        &agent(),
        &[
            ("resource_id", RESOURCE_TARIFFS),
            ("filters", &filters),
            ("sort", "DatInicioVigencia desc"),
            ("limit", "2000"),
        ],
    )?;
    let periods = parse_tariff_records(&records);
    if periods.is_empty() {
        return Err(format!("ANEEL: nenhuma tarifa B1 residencial encontrada para '{distributor}'"));
    }
    Ok(periods)
}

pub fn fetch_flags() -> Result<Vec<FlagMonth>, String> {
    let records = datastore(
        &agent(),
        &[
            ("resource_id", RESOURCE_FLAGS),
            ("sort", "DatCompetencia desc"),
            ("limit", "1000"),
        ],
    )?;
    let flags = parse_flag_records(&records);
    if flags.is_empty() {
        return Err("ANEEL: nenhuma bandeira encontrada".to_string());
    }
    Ok(flags)
}

/// Inserts the built-in ENEL CE periods when nothing is cached for that distributor.
pub fn seed_defaults(conn: &Connection, distributor: &str) -> rusqlite::Result<()> {
    if distributor != "ENEL CE" {
        return Ok(());
    }
    for (start, end, tusd, te, reh) in SEED_ENEL_CE {
        conn.execute(
            "INSERT OR IGNORE INTO tariff_periods (distributor, start_date, end_date, tusd_mwh, te_mwh, resolution, origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'embutido')",
            params![distributor, start, end, tusd, te, reh],
        )?;
    }
    Ok(())
}

pub fn store_tariffs(conn: &mut Connection, distributor: &str, periods: &[TariffPeriod]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for p in periods {
        tx.execute(
            "INSERT INTO tariff_periods (distributor, start_date, end_date, tusd_mwh, te_mwh, resolution, origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'aneel') \
             ON CONFLICT(distributor, start_date) DO UPDATE SET end_date = excluded.end_date, \
               tusd_mwh = excluded.tusd_mwh, te_mwh = excluded.te_mwh, resolution = excluded.resolution, \
               origin = 'aneel'",
            params![
                distributor,
                p.start.format("%Y-%m-%d").to_string(),
                p.end.format("%Y-%m-%d").to_string(),
                p.tusd_mwh,
                p.te_mwh,
                p.resolution
            ],
        )?;
    }
    tx.commit()
}

pub fn store_flags(conn: &mut Connection, flags: &[FlagMonth]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for f in flags {
        tx.execute(
            "INSERT INTO tariff_flags (month, flag, add_mwh) VALUES (?1, ?2, ?3) \
             ON CONFLICT(month) DO UPDATE SET flag = excluded.flag, add_mwh = excluded.add_mwh",
            params![f.month, f.flag, f.add_mwh],
        )?;
    }
    tx.commit()
}

/// Outcome of a refresh, also persisted in `billing_kv` (`tariff_last_ok`, `tariff_last_error`).
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub periods: usize,
    pub flags: usize,
    pub errors: Vec<String>,
}

/// Downloads tariffs and flags and caches them. Partial failures keep the previous cache.
pub fn refresh(conn: &mut Connection, distributor: &str) -> RefreshOutcome {
    let mut outcome = RefreshOutcome {
        periods: 0,
        flags: 0,
        errors: Vec::new(),
    };
    match fetch_tariffs(distributor) {
        Ok(periods) => match store_tariffs(conn, distributor, &periods) {
            Ok(()) => outcome.periods = periods.len(),
            Err(e) => outcome.errors.push(format!("cache tarifas: {e}")),
        },
        Err(e) => outcome.errors.push(e),
    }
    match fetch_flags() {
        Ok(flags) => match store_flags(conn, &flags) {
            Ok(()) => outcome.flags = flags.len(),
            Err(e) => outcome.errors.push(format!("cache bandeiras: {e}")),
        },
        Err(e) => outcome.errors.push(e),
    }
    let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
    if outcome.errors.is_empty() {
        let _ = kv_set(conn, "tariff_last_ok", &now);
        let _ = kv_set(conn, "tariff_last_error", "");
    } else {
        let _ = kv_set(conn, "tariff_last_error", &format!("{now}: {}", outcome.errors.join(" | ")));
        if outcome.periods > 0 && outcome.flags > 0 {
            let _ = kv_set(conn, "tariff_last_ok", &now);
        }
    }
    let _ = kv_set(conn, "tariff_last_attempt", &now);
    outcome
}

/// Price of one kWh on a given day, with the full breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct DayPrice {
    /// TE + TUSD (R$/kWh, no taxes).
    pub base_kwh: f64,
    pub flag: String,
    /// Flag surcharge (R$/kWh, no taxes).
    pub flag_kwh: f64,
    /// The month flag is not published yet: the last known one was used.
    pub flag_provisional: bool,
    /// The day is outside every known tariff period: nearest period used.
    pub period_estimated: bool,
    pub resolution: String,
    /// ICMS + PIS/COFINS rate applied "por dentro".
    pub tax_rate: f64,
    /// Final R$/kWh with taxes.
    pub final_kwh: f64,
}

/// In-memory copy of the cached tariffs, for computing many days quickly.
#[derive(Debug, Clone, Default)]
pub struct TariffBook {
    pub periods: Vec<TariffPeriod>,
    pub flags: BTreeMap<String, (String, f64)>,
    pub origin: String,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
}

impl TariffBook {
    /// Read-only: never writes (the UI calls it every few seconds).
    pub fn load(conn: &Connection, distributor: &str) -> rusqlite::Result<Self> {
        let mut periods = Vec::new();
        let mut origins = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT start_date, end_date, tusd_mwh, te_mwh, COALESCE(resolution, ''), origin \
                 FROM tariff_periods WHERE distributor = ?1 ORDER BY start_date",
            )?;
            let rows = stmt.query_map(params![distributor], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (s, e, tusd, te, reh, origin) = row?;
                if let (Ok(start), Ok(end)) = (
                    NaiveDate::parse_from_str(&s, "%Y-%m-%d"),
                    NaiveDate::parse_from_str(&e, "%Y-%m-%d"),
                ) {
                    periods.push(TariffPeriod {
                        start,
                        end,
                        tusd_mwh: tusd,
                        te_mwh: te,
                        resolution: reh,
                    });
                    origins.push(origin);
                }
            }
        }
        let mut flags = BTreeMap::new();
        {
            let mut stmt = conn.prepare("SELECT month, flag, add_mwh FROM tariff_flags")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
            })?;
            for row in rows {
                let (m, f, a) = row?;
                flags.insert(m, (f, a));
            }
        }
        if periods.is_empty() && distributor == "ENEL CE" {
            for (start, end, tusd, te, reh) in SEED_ENEL_CE {
                if let (Ok(start), Ok(end)) = (
                    NaiveDate::parse_from_str(start, "%Y-%m-%d"),
                    NaiveDate::parse_from_str(end, "%Y-%m-%d"),
                ) {
                    periods.push(TariffPeriod {
                        start,
                        end,
                        tusd_mwh: *tusd,
                        te_mwh: *te,
                        resolution: reh.to_string(),
                    });
                    origins.push("embutido".to_string());
                }
            }
        }
        let origin = if origins.iter().any(|o| o == "aneel") {
            "ANEEL (dados abertos)".to_string()
        } else if periods.is_empty() {
            "nenhuma".to_string()
        } else {
            "valores embutidos (sem conexão com a ANEEL ainda)".to_string()
        };
        Ok(Self {
            periods,
            flags,
            origin,
            last_ok: kv_get(conn, "tariff_last_ok").filter(|s| !s.is_empty()),
            last_error: kv_get(conn, "tariff_last_error").filter(|s| !s.is_empty()),
        })
    }

    pub fn period_for(&self, day: NaiveDate) -> Option<(&TariffPeriod, bool)> {
        if let Some(p) = self
            .periods
            .iter()
            .rev()
            .find(|p| p.start <= day && day <= p.end)
        {
            return Some((p, false));
        }
        // Outside every period: latest one that started before the day, else the earliest.
        self.periods
            .iter()
            .rev()
            .find(|p| p.start <= day)
            .or_else(|| self.periods.first())
            .map(|p| (p, true))
    }

    pub fn flag_for(&self, day: NaiveDate) -> (String, f64, bool) {
        let month = format!("{:04}-{:02}", day.year(), day.month());
        if let Some((f, a)) = self.flags.get(&month) {
            return (f.clone(), *a / 1000.0, false);
        }
        match self.flags.range(..month).next_back() {
            Some((_, (f, a))) => (f.clone(), *a / 1000.0, true),
            None => ("Verde".to_string(), 0.0, true),
        }
    }

    /// Final price of a kWh on `day`; None when no tariff at all is known.
    pub fn price_for(&self, day: NaiveDate, settings: &BillingSettings) -> Option<DayPrice> {
        let (period, estimated) = self.period_for(day)?;
        let (flag, flag_kwh, provisional) = self.flag_for(day);
        let tax_rate = settings.tax_rate();
        let base = period.base_kwh();
        Some(DayPrice {
            base_kwh: base,
            flag,
            flag_kwh,
            flag_provisional: provisional,
            period_estimated: estimated,
            resolution: period.resolution.clone(),
            tax_rate,
            final_kwh: (base + flag_kwh) / (1.0 - tax_rate),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn br_numbers() {
        assert_eq!(parse_br_number("488,49"), Some(488.49));
        assert_eq!(parse_br_number(",00"), Some(0.0));
        assert_eq!(parse_br_number("1.234,5"), Some(1234.5));
        assert_eq!(parse_br_number("-38,13"), Some(-38.13));
        assert_eq!(parse_br_number(""), None);
    }

    fn rec(base: &str, modal: &str, sub: &str, detalhe: &str, ini: &str, fim: &str, tusd: &str, te: &str) -> Value {
        json!({
            "DscREH": "RESOLUÇÃO HOMOLOGATÓRIA Nº 3.324",
            "DatInicioVigencia": ini, "DatFimVigencia": fim,
            "DscBaseTarifaria": base, "DscSubGrupo": "B1", "DscModalidadeTarifaria": modal,
            "DscClasse": "Residencial", "DscSubClasse": sub, "DscDetalhe": detalhe,
            "NomPostoTarifario": "Não se aplica", "DscUnidadeTerciaria": "MWh",
            "SigAgenteAcessante": "Não se aplica", "VlrTUSD": tusd, "VlrTE": te
        })
    }

    #[test]
    fn keeps_only_residential_application_tariff() {
        let records = vec![
            rec("Tarifa de Aplicação", "Convencional", "Residencial", "Não se aplica", "2026-08-26", "2027-04-21", "488,49", "213,26"),
            rec("Base Econômica", "Convencional", "Residencial", "Não se aplica", "2026-08-26", "2027-04-21", "492,54", "280,05"),
            rec("Tarifa de Aplicação", "Convencional", "Baixa Renda", "Não se aplica", "2026-08-26", "2027-04-21", "380,66", "203,15"),
            rec("Tarifa de Aplicação", "Convencional", "Residencial", "SCEE", "2026-08-26", "2027-04-21", "488,49", "-38,13"),
            rec("Tarifa de Aplicação", "Convencional pré-pagamento", "Residencial", "Não se aplica", "2026-08-26", "2027-04-21", "1,00", "1,00"),
            rec("Tarifa de Aplicação", "Convencional", "Residencial", "Não se aplica", "2026-04-22", "2026-08-25", "488,78", "260,99"),
        ];
        let periods = parse_tariff_records(&records);
        assert_eq!(periods.len(), 2);
        assert_eq!(periods[1].tusd_mwh, 488.49);
        assert_eq!(periods[1].te_mwh, 213.26);
        assert!((periods[1].base_kwh() - 0.70175).abs() < 1e-9);
    }

    #[test]
    fn flags_and_prices() {
        let flags = parse_flag_records(&[
            json!({"DatCompetencia": "2026-10-01", "NomBandeiraAcionada": "Verde", "VlrAdicionalBandeira": ",00"}),
            json!({"DatCompetencia": "2026-09-01", "NomBandeiraAcionada": "Amarela", "VlrAdicionalBandeira": "18,85"}),
        ]);
        assert_eq!(flags.len(), 2);
        let mut book = TariffBook::default();
        book.periods = vec![TariffPeriod {
            start: NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
            end: NaiveDate::from_ymd_opt(2027, 4, 21).unwrap(),
            tusd_mwh: 488.49,
            te_mwh: 213.26,
            resolution: "REH".into(),
        }];
        for f in flags {
            book.flags.insert(f.month, (f.flag, f.add_mwh));
        }
        let s = BillingSettings::default();
        let sep = book.price_for(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(), &s).unwrap();
        assert_eq!(sep.flag, "Amarela");
        assert!(!sep.flag_provisional);
        assert!((sep.final_kwh - (0.70175 + 0.01885) / (1.0 - 0.257)).abs() < 1e-9);
        let nov = book.price_for(NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(), &s).unwrap();
        assert_eq!(nov.flag, "Verde");
        assert!(nov.flag_provisional);
        let later = book.price_for(NaiveDate::from_ymd_opt(2027, 6, 1).unwrap(), &s).unwrap();
        assert!(later.period_estimated);
    }

    #[test]
    fn seeds_when_empty() {
        let conn = Connection::open_in_memory().unwrap();
        crate::billing::ensure_schema(&conn).unwrap();
        let book = TariffBook::load(&conn, "ENEL CE").unwrap();
        assert_eq!(book.periods.len(), SEED_ENEL_CE.len());
        assert!(book.origin.starts_with("valores embutidos"));
    }
}
