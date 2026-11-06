//! Per-billing-month costs and the compact Markdown export (meant to be pasted into an AI chat).
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate};
use rusqlite::Connection;

use super::{
    BillingSettings, DayUsage, first_recorded_day, load_daily_usage, load_psu_efficiency, load_settings,
    tariff::{DayPrice, TariffBook},
};

/// Totals of one billing month.
#[derive(Debug, Clone, Default)]
pub struct MonthReport {
    /// `YYYY-MM` of the month the cycle ends in (the month printed on the bill).
    pub label: String,
    pub start: Option<NaiveDate>,
    /// Inclusive.
    pub end: Option<NaiveDate>,
    pub is_current: bool,
    pub days_with_data: u32,
    pub pc_measured_kwh: f64,
    /// Wall energy of the PC (PSU losses + extra watts applied).
    pub pc_wall_kwh: f64,
    pub pc_on_hours: f64,
    pub monitor_kwh: f64,
    pub monitor_on_hours: f64,
    pub cost_pc: f64,
    pub cost_monitor: f64,
    pub flags: Vec<String>,
    pub provisional: bool,
    /// For the current month: projected total cost for the whole cycle.
    pub projection: Option<f64>,
    /// Elapsed fraction of the cycle (current month only).
    pub elapsed_days: f64,
    pub cycle_days: f64,
    /// Measured kWh per component (`cpu_data`, ...).
    pub components: Vec<(String, f64)>,
}

impl MonthReport {
    pub fn total_kwh(&self) -> f64 {
        self.pc_wall_kwh + self.monitor_kwh
    }
    pub fn total_cost(&self) -> f64 {
        self.cost_pc + self.cost_monitor
    }
    pub fn avg_price(&self) -> f64 {
        let kwh = self.total_kwh();
        if kwh > 0.0 { self.total_cost() / kwh } else { 0.0 }
    }
}

#[derive(Debug, Clone)]
pub struct BillingReport {
    pub generated: DateTime<Local>,
    pub settings: BillingSettings,
    pub psu_eta: f64,
    pub tariff: TariffBook,
    /// Price for today (None if no tariff is known at all).
    pub today_price: Option<DayPrice>,
    /// Newest first.
    pub months: Vec<MonthReport>,
}

impl BillingReport {
    pub fn current_month(&self) -> Option<&MonthReport> {
        self.months.iter().find(|m| m.is_current)
    }
}

/// Start (inclusive) and end (inclusive) of the billing cycle containing `day`, plus its label.
pub fn cycle_for(day: NaiveDate, billing_day: u32) -> (NaiveDate, NaiveDate, String) {
    let b = billing_day.clamp(1, 28);
    let first_of = |y: i32, m: u32| NaiveDate::from_ymd_opt(y, m, 1).expect("valid month");
    let add_month = |y: i32, m: u32| if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let sub_month = |y: i32, m: u32| if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let (y, m) = (day.year(), day.month());
    if b == 1 {
        let start = first_of(y, m);
        let (ny, nm) = add_month(y, m);
        let end = first_of(ny, nm) - ChronoDuration::days(1);
        return (start, end, format!("{y:04}-{m:02}"));
    }
    let (sy, sm) = if day.day() >= b { (y, m) } else { sub_month(y, m) };
    let start = NaiveDate::from_ymd_opt(sy, sm, b).expect("valid day");
    let (ey, em) = add_month(sy, sm);
    let end = NaiveDate::from_ymd_opt(ey, em, b).expect("valid day") - ChronoDuration::days(1);
    (start, end, format!("{ey:04}-{em:02}"))
}

/// Builds the report for every month with data (newest first, at most `max_months`).
pub fn build(conn: &Connection, max_months: usize) -> rusqlite::Result<BillingReport> {
    let settings = load_settings(conn);
    let psu_eta = load_psu_efficiency(conn);
    let tariff = TariffBook::load(conn, &settings.distributor)?;
    let now = Local::now();
    let today = now.date_naive();
    let today_price = tariff.price_for(today, &settings);

    let (cur_start, cur_end, _) = cycle_for(today, settings.billing_day);
    // Oldest cycle we need: max_months back, but not before the first recorded day.
    let mut oldest = cur_start;
    for _ in 1..max_months.max(1) {
        let (s, _, _) = cycle_for(oldest - ChronoDuration::days(1), settings.billing_day);
        oldest = s;
    }
    if let Some(first) = first_recorded_day(conn).and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()) {
        let (s, _, _) = cycle_for(first, settings.billing_day);
        if s > oldest {
            oldest = s;
        }
    }

    let days = load_daily_usage(
        conn,
        &oldest.format("%Y-%m-%d").to_string(),
        &cur_end.format("%Y-%m-%d").to_string(),
    )?;

    let mut months: Vec<MonthReport> = Vec::new();
    for (day_str, usage) in days {
        let Ok(day) = NaiveDate::parse_from_str(&day_str, "%Y-%m-%d") else {
            continue;
        };
        let (start, end, label) = cycle_for(day, settings.billing_day);
        if months.last().map_or(true, |m| m.label != label) {
            months.push(MonthReport {
                label: label.clone(),
                start: Some(start),
                end: Some(end),
                is_current: start == cur_start,
                cycle_days: (end - start).num_days() as f64 + 1.0,
                ..Default::default()
            });
        }
        let month = months.last_mut().expect("just pushed");
        add_day(month, &usage, day, &settings, psu_eta, &tariff);
    }

    // Make sure the current month exists even before the first sample of the cycle.
    if !months.iter().any(|m| m.is_current) {
        let (start, end, label) = cycle_for(today, settings.billing_day);
        months.push(MonthReport {
            label,
            start: Some(start),
            end: Some(end),
            is_current: true,
            cycle_days: (end - start).num_days() as f64 + 1.0,
            ..Default::default()
        });
    }

    for m in months.iter_mut().filter(|m| m.is_current) {
        if let Some(start) = m.start {
            let start_dt = start.and_hms_opt(0, 0, 0).expect("midnight");
            let elapsed = (now.naive_local() - start_dt).num_seconds().max(0) as f64 / 86_400.0;
            m.elapsed_days = elapsed.min(m.cycle_days);
            if m.elapsed_days >= 0.25 && m.total_cost() > 0.0 {
                m.projection = Some(m.total_cost() / m.elapsed_days * m.cycle_days);
            }
        }
    }

    months.sort_by(|a, b| b.label.cmp(&a.label));
    months.truncate(max_months.max(1));
    Ok(BillingReport {
        generated: now,
        settings,
        psu_eta,
        tariff,
        today_price,
        months,
    })
}

fn add_day(
    month: &mut MonthReport,
    usage: &DayUsage,
    day: NaiveDate,
    settings: &BillingSettings,
    psu_eta: f64,
    tariff: &TariffBook,
) {
    let pc_measured = usage.pc_measured_wh / 1000.0;
    let pc_wall = pc_measured / psu_eta + settings.extra_watts * usage.pc_on_hours / 1000.0;
    let monitor = usage.monitor_wh / 1000.0;
    if pc_measured > 0.0 || monitor > 0.0 {
        month.days_with_data += 1;
    }
    month.pc_measured_kwh += pc_measured;
    month.pc_wall_kwh += pc_wall;
    month.pc_on_hours += usage.pc_on_hours;
    month.monitor_kwh += monitor;
    month.monitor_on_hours += usage.monitor_on_hours;
    for (name, wh) in &usage.components {
        match month.components.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v += wh / 1000.0,
            None => month.components.push((name.clone(), wh / 1000.0)),
        }
    }
    if let Some(price) = tariff.price_for(day, settings) {
        month.cost_pc += pc_wall * price.final_kwh;
        month.cost_monitor += monitor * price.final_kwh;
        if !month.flags.contains(&price.flag) {
            month.flags.push(price.flag.clone());
        }
        if price.flag_provisional || price.period_estimated {
            month.provisional = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------------

/// Brazilian number format: `1.234,56`.
pub fn fmt_br(value: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, value.abs());
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s.clone(), None),
    };
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(c);
    }
    let sign = if value < 0.0 && value.abs() >= 0.5 * 10f64.powi(-(decimals as i32)) {
        "-"
    } else {
        ""
    };
    match frac {
        Some(f) => format!("{sign}{grouped},{f}"),
        None => format!("{sign}{grouped}"),
    }
}

pub fn component_label(table: &str) -> &str {
    match table {
        "cpu_data" => "CPU",
        "gpu_data" => "GPU",
        "ram_data" => "RAM",
        "disk_data" => "Disco",
        "network_data" => "Rede",
        other => other,
    }
}

fn short_date(d: Option<NaiveDate>) -> String {
    d.map(|d| d.format("%d/%m").to_string()).unwrap_or_default()
}

/// Compact Markdown: only what matters to understand the bill share of the PC.
pub fn to_markdown(r: &BillingReport) -> String {
    let s = &r.settings;
    let mut md = String::new();
    md.push_str(&format!("# Consumo do PC na conta de luz — {}\n\n", s.region_label));
    md.push_str(&format!(
        "Gerado em {} pelo WattSeal (fork). Valores estimados por sensores; custo marginal (sem CIP e sem custo de disponibilidade).\n\n",
        r.generated.format("%d/%m/%Y %H:%M")
    ));

    md.push_str("## Premissas\n");
    match &r.today_price {
        Some(p) => md.push_str(&format!(
            "- Tarifa {} B1 residencial hoje: R$ {}/kWh com impostos (TE+TUSD R$ {} + bandeira {} R$ {}; ICMS {}% + PIS/COFINS {}%). Fonte: {}.\n",
            s.distributor,
            fmt_br(p.final_kwh, 4),
            fmt_br(p.base_kwh, 4),
            p.flag,
            fmt_br(p.flag_kwh, 4),
            fmt_br(s.icms_pct, 1),
            fmt_br(s.pis_cofins_pct, 2),
            r.tariff.origin
        )),
        None => md.push_str("- Tarifa: indisponível (sem dados da ANEEL).\n"),
    }
    md.push_str(&format!(
        "- PC: energia medida ÷ eficiência da fonte {}%{}.\n",
        fmt_br(r.psu_eta * 100.0, 0),
        if s.extra_watts > 0.0 {
            format!(" + {} W fixos enquanto ligado", fmt_br(s.extra_watts, 0))
        } else {
            String::new()
        }
    ));
    if s.monitor_enabled {
        md.push_str(&format!(
            "- Monitor: {}–{} W conforme o brilho (DDC/CI), {} W em espera/desligado.\n",
            fmt_br(s.monitor_w_min, 0),
            fmt_br(s.monitor_w_max, 0),
            fmt_br(s.monitor_w_standby, 1)
        ));
    }
    if s.billing_day > 1 {
        md.push_str(&format!("- Ciclo de leitura começa no dia {} de cada mês.\n", s.billing_day));
    }

    md.push_str("\n## Por mês\n");
    md.push_str("| Mês | Período | PC ligado (h) | PC kWh | Monitor kWh (h ligado) | Total kWh | R$/kWh | Bandeira | PC R$ | Monitor R$ | Total R$ |\n");
    md.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    for m in &r.months {
        md.push_str(&format!(
            "| {}{} | {}–{} | {} | {} | {} ({}) | {} | {} | {} | {} | {} | **{}** |\n",
            m.label,
            if m.is_current { "*" } else { "" },
            short_date(m.start),
            short_date(m.end),
            fmt_br(m.pc_on_hours, 1),
            fmt_br(m.pc_wall_kwh, 2),
            fmt_br(m.monitor_kwh, 2),
            fmt_br(m.monitor_on_hours, 1),
            fmt_br(m.total_kwh(), 2),
            fmt_br(m.avg_price(), 3),
            if m.flags.is_empty() { "-".to_string() } else { m.flags.join("/") },
            fmt_br(m.cost_pc, 2),
            fmt_br(m.cost_monitor, 2),
            fmt_br(m.total_cost(), 2),
        ));
    }
    if let Some(cur) = r.current_month() {
        md.push_str(&format!(
            "\n\\* mês em andamento ({} de {} dias){}.\n",
            fmt_br(cur.elapsed_days, 1),
            fmt_br(cur.cycle_days, 0),
            cur.projection
                .map(|p| format!(", projeção para o ciclo: R$ {}", fmt_br(p, 2)))
                .unwrap_or_default()
        ));
    }
    if r.months.iter().any(|m| m.provisional) {
        md.push_str("Bandeira ou tarifa ainda não publicada pela ANEEL em algum mês: usado o último valor conhecido.\n");
    }

    let component_lines: Vec<String> = r
        .months
        .iter()
        .filter_map(|m| {
            let mut comps = m.components.clone();
            comps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let list: Vec<String> = comps
                .iter()
                .filter(|(_, kwh)| *kwh >= 0.005)
                .map(|(n, kwh)| format!("{} {}", component_label(n), fmt_br(*kwh, 2)))
                .collect();
            (!list.is_empty()).then(|| format!("- {}: {}\n", m.label, list.join(" · ")))
        })
        .collect();
    if !component_lines.is_empty() {
        md.push_str("\n## Componentes (kWh medidos, antes das perdas da fonte)\n");
        for line in component_lines {
            md.push_str(&line);
        }
    }
    md
}

/// Writes the Markdown export to `relatorios/` and returns its path.
pub fn export_markdown(conn: &Connection, max_months: usize) -> Result<PathBuf, String> {
    let report = build(conn, max_months).map_err(|e| e.to_string())?;
    let dir = crate::paths::reports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("consumo-pc-{}.md", report.generated.format("%Y-%m-%d")));
    std::fs::write(&path, to_markdown(&report)).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_cycle() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let (s, e, l) = cycle_for(d, 1);
        assert_eq!((s.day(), e.day(), l.as_str()), (1, 30, "2026-11"));
        let (s, e, _) = cycle_for(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(), 1);
        assert_eq!((s.month(), e.day()), (12, 31));
    }

    #[test]
    fn reading_day_cycle() {
        let (s, e, l) = cycle_for(NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(), 15);
        assert_eq!(s, NaiveDate::from_ymd_opt(2026, 10, 15).unwrap());
        assert_eq!(e, NaiveDate::from_ymd_opt(2026, 11, 14).unwrap());
        assert_eq!(l, "2026-11");
        let (s, _, l) = cycle_for(NaiveDate::from_ymd_opt(2026, 12, 20).unwrap(), 15);
        assert_eq!(s, NaiveDate::from_ymd_opt(2026, 12, 15).unwrap());
        assert_eq!(l, "2027-01");
    }

    #[test]
    fn brazilian_format() {
        assert_eq!(fmt_br(1234.567, 2), "1.234,57");
        assert_eq!(fmt_br(0.9483, 4), "0,9483");
        assert_eq!(fmt_br(12.0, 0), "12");
        assert_eq!(fmt_br(1_000_000.0, 1), "1.000.000,0");
    }

    #[test]
    fn report_from_recorded_samples() {
        use std::time::Duration;

        use crate::{
            billing::{ensure_schema, record_sample},
            types::{ComputedSensorData, EnergyUj, MetricSample, TotalData},
        };
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        // 1 kWh measured today
        let sample = MetricSample::new(
            std::time::SystemTime::now(),
            Duration::from_secs(1),
            vec![ComputedSensorData::Total(TotalData {
                total_energy: EnergyUj::from_joules(3_600_000.0),
            })],
        );
        record_sample(&conn, &sample).unwrap();
        let report = build(&conn, 12).unwrap();
        let cur = report.current_month().unwrap();
        assert!((cur.pc_wall_kwh - 1.0).abs() < 1e-9);
        let price = report.today_price.clone().unwrap();
        assert!((cur.total_cost() - price.final_kwh).abs() < 1e-9);
        let md = to_markdown(&report);
        assert!(md.contains("| Mês |"));
        assert!(md.lines().count() < 40);
    }
}
