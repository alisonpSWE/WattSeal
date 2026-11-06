//! Fork Crateús/CE: "Conta de luz" page — monthly cost of the PC + monitor on the electricity bill.
use common::{
    Database, MonitorPowerState,
    billing::{
        self, BillingSettings, MonitorStatus,
        report::{self, BillingReport, MonthReport},
        tariff,
    },
};
use iced::{
    Alignment, Element, Length, Padding, Task,
    widget::{Column, Container, Row, Scrollable, Text, button, text_input, toggler},
};

use crate::{
    components::helpers::separator,
    message::Message,
    styles::{
        button::ButtonStyle,
        container::ContainerStyle,
        scrollable::ScrollableStyle,
        style_constants::{
            FONT_BOLD, FONT_SIZE_BODY, FONT_SIZE_LARGE, FONT_SIZE_SMALL, FONT_SIZE_SUBTITLE, FONT_SIZE_TITLE,
            PADDING_LARGE, PADDING_MEDIUM, SPACING_LARGE, SPACING_MEDIUM, SPACING_SMALL, SPACING_XLARGE,
        },
        text::TextStyle,
        toggler::TogglerStyle,
    },
    themes::AppTheme,
    translations::format_number,
    types::AppLanguage,
};

/// Months shown on the page / exported.
pub const REPORT_MONTHS: usize = 12;

/// Picks the Portuguese text when the UI is in Portuguese, English otherwise.
pub fn t(language: AppLanguage, pt: &'static str, en: &'static str) -> &'static str {
    if language == AppLanguage::Portuguese { pt } else { en }
}

pub fn page_name(language: AppLanguage) -> &'static str {
    t(language, "Conta de luz", "Electricity bill")
}

/// Editable fields of the bill settings form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillField {
    Distributor,
    Region,
    Icms,
    PisCofins,
    BillingDay,
    ExtraWatts,
    MonitorMin,
    MonitorMax,
    MonitorStandby,
    MonitorDefaultBrightness,
    MonitorMatch,
}

/// Text inputs mirror of [`BillingSettings`].
#[derive(Debug, Clone, Default)]
pub struct BillInputs {
    pub distributor: String,
    pub region: String,
    pub icms: String,
    pub pis_cofins: String,
    pub billing_day: String,
    pub extra_watts: String,
    pub monitor_min: String,
    pub monitor_max: String,
    pub monitor_standby: String,
    pub monitor_default_brightness: String,
    pub monitor_match: String,
    pub monitor_enabled: bool,
    pub use_auto_tariff: bool,
}

fn num_to_input(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    s.replace('.', ",")
}

fn parse_input(s: &str) -> Option<f64> {
    s.trim().replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite())
}

impl BillInputs {
    fn from_settings(s: &BillingSettings) -> Self {
        Self {
            distributor: s.distributor.clone(),
            region: s.region_label.clone(),
            icms: num_to_input(s.icms_pct),
            pis_cofins: num_to_input(s.pis_cofins_pct),
            billing_day: s.billing_day.to_string(),
            extra_watts: num_to_input(s.extra_watts),
            monitor_min: num_to_input(s.monitor_w_min),
            monitor_max: num_to_input(s.monitor_w_max),
            monitor_standby: num_to_input(s.monitor_w_standby),
            monitor_default_brightness: num_to_input(s.monitor_default_brightness),
            monitor_match: s.monitor_match.clone(),
            monitor_enabled: s.monitor_enabled,
            use_auto_tariff: s.use_auto_tariff,
        }
    }

    /// Validated settings, or the first invalid field.
    fn to_settings(&self) -> Result<BillingSettings, &'static str> {
        let pct = |s: &str, max: f64| parse_input(s).filter(|v| *v >= 0.0 && *v <= max);
        let watts = |s: &str| parse_input(s).filter(|v| *v >= 0.0 && *v <= 500.0);
        let s = BillingSettings {
            distributor: if self.distributor.trim().is_empty() {
                return Err("distribuidora");
            } else {
                self.distributor.trim().to_uppercase()
            },
            region_label: self.region.trim().to_string(),
            icms_pct: pct(&self.icms, 50.0).ok_or("ICMS")?,
            pis_cofins_pct: pct(&self.pis_cofins, 30.0).ok_or("PIS/COFINS")?,
            billing_day: parse_input(&self.billing_day)
                .filter(|v| *v >= 1.0 && *v <= 28.0 && v.fract() == 0.0)
                .ok_or("dia de leitura (1-28)")? as u32,
            extra_watts: watts(&self.extra_watts).ok_or("W extras")?,
            monitor_enabled: self.monitor_enabled,
            monitor_w_min: watts(&self.monitor_min).ok_or("monitor W mín.")?,
            monitor_w_max: watts(&self.monitor_max).ok_or("monitor W máx.")?,
            monitor_w_standby: parse_input(&self.monitor_standby)
                .filter(|v| *v >= 0.0 && *v <= 50.0)
                .ok_or("monitor W espera")?,
            monitor_default_brightness: pct(&self.monitor_default_brightness, 100.0).ok_or("brilho padrão")?,
            monitor_match: self.monitor_match.trim().to_string(),
            use_auto_tariff: self.use_auto_tariff,
        };
        if s.monitor_w_max < s.monitor_w_min {
            return Err("monitor W máx. < mín.");
        }
        Ok(s)
    }
}

/// State of the bill page (and of the dashboard "this month" tile).
#[derive(Debug, Clone, Default)]
pub struct BillState {
    pub report: Option<BillingReport>,
    pub monitor: Option<MonitorStatus>,
    pub inputs: BillInputs,
    pub status: Option<String>,
    pub refreshing: bool,
    pub settings_open: bool,
}

impl BillState {
    pub fn load(database: &Database) -> Self {
        let mut state = Self {
            inputs: BillInputs::from_settings(&billing::load_settings(database.connection())),
            ..Default::default()
        };
        state.refresh(database);
        state
    }

    pub fn refresh(&mut self, database: &Database) {
        match report::build(database.connection(), REPORT_MONTHS) {
            Ok(r) => self.report = Some(r),
            Err(e) => common::clog!("✗ Failed to build bill report: {e}"),
        }
        self.monitor = billing::read_monitor_status(database.connection());
    }

    /// Month-to-date cost for the dashboard when the automatic tariff is enabled.
    pub fn dashboard_month_cost(&self) -> Option<f64> {
        let r = self.report.as_ref()?;
        if !r.settings.use_auto_tariff || r.today_price.is_none() {
            return None;
        }
        r.current_month().map(MonthReport::total_cost)
    }

    pub fn update(&mut self, message: BillMessage, database: &mut Database, language: AppLanguage) -> Task<Message> {
        match message {
            BillMessage::Input(field, value) => {
                let i = &mut self.inputs;
                match field {
                    BillField::Distributor => i.distributor = value,
                    BillField::Region => i.region = value,
                    BillField::Icms => i.icms = value,
                    BillField::PisCofins => i.pis_cofins = value,
                    BillField::BillingDay => i.billing_day = value,
                    BillField::ExtraWatts => i.extra_watts = value,
                    BillField::MonitorMin => i.monitor_min = value,
                    BillField::MonitorMax => i.monitor_max = value,
                    BillField::MonitorStandby => i.monitor_standby = value,
                    BillField::MonitorDefaultBrightness => i.monitor_default_brightness = value,
                    BillField::MonitorMatch => i.monitor_match = value,
                }
                Task::none()
            }
            BillMessage::ToggleMonitor(v) => {
                self.inputs.monitor_enabled = v;
                Task::none()
            }
            BillMessage::ToggleAutoTariff(v) => {
                self.inputs.use_auto_tariff = v;
                Task::none()
            }
            BillMessage::ToggleSettings => {
                self.settings_open = !self.settings_open;
                Task::none()
            }
            BillMessage::Save => {
                match self.inputs.to_settings() {
                    Ok(settings) => match billing::save_settings(database.connection(), &settings) {
                        Ok(()) => {
                            self.inputs = BillInputs::from_settings(&settings);
                            self.status = Some(t(language, "Ajustes salvos.", "Settings saved.").to_string());
                            self.refresh(database);
                        }
                        Err(e) => self.status = Some(format!("✗ {e}")),
                    },
                    Err(field) => {
                        self.status = Some(format!("✗ {}: {field}", t(language, "Valor inválido", "Invalid value")))
                    }
                }
                Task::none()
            }
            BillMessage::ResetDefaults => {
                self.inputs = BillInputs::from_settings(&BillingSettings::default());
                self.status = Some(
                    t(
                        language,
                        "Valores padrão carregados (clique em Salvar para aplicar).",
                        "Defaults loaded (press Save to apply).",
                    )
                    .to_string(),
                );
                Task::none()
            }
            BillMessage::RefreshTariff => {
                if self.refreshing {
                    return Task::none();
                }
                self.refreshing = true;
                self.status = Some(t(language, "Consultando a ANEEL…", "Querying ANEEL…").to_string());
                let distributor = billing::load_settings(database.connection()).distributor;
                Task::perform(async move { refresh_tariff_blocking(distributor) }, |result| {
                    Message::Bill(BillMessage::TariffRefreshed(result))
                })
            }
            BillMessage::TariffRefreshed(result) => {
                self.refreshing = false;
                self.status = Some(match result {
                    Ok(counts) => format!(
                        "✓ {} ({counts})",
                        t(language, "Tarifa atualizada pela ANEEL", "Tariff updated from ANEEL")
                    ),
                    Err(e) => format!("✗ {e}"),
                });
                self.refresh(database);
                Task::none()
            }
            BillMessage::ExportMarkdown => {
                match report::export_markdown(database.connection(), REPORT_MONTHS) {
                    Ok(path) => {
                        self.status = Some(format!("✓ {}", path.display()));
                        open::that(&path).ok();
                    }
                    Err(e) => self.status = Some(format!("✗ {e}")),
                }
                Task::none()
            }
            BillMessage::CopyMarkdown => match report::build(database.connection(), REPORT_MONTHS) {
                Ok(r) => {
                    self.status = Some(
                        t(
                            language,
                            "✓ Resumo em Markdown copiado — cole na IA.",
                            "✓ Markdown summary copied to the clipboard.",
                        )
                        .to_string(),
                    );
                    iced::clipboard::write(report::to_markdown(&r))
                }
                Err(e) => {
                    self.status = Some(format!("✗ {e}"));
                    Task::none()
                }
            },
            BillMessage::OpenReports => {
                let dir = common::paths::reports_dir();
                let _ = std::fs::create_dir_all(&dir);
                open::that(dir).ok();
                Task::none()
            }
        }
    }
}

/// Downloads the ANEEL tariff with a dedicated connection (runs on the async executor).
fn refresh_tariff_blocking(distributor: String) -> Result<String, String> {
    let mut db = Database::open_without_migrations().map_err(|e| e.to_string())?;
    let outcome = tariff::refresh(db.connection_mut(), &distributor);
    if outcome.errors.is_empty() {
        Ok(format!("{} / {}", outcome.periods, outcome.flags))
    } else {
        Err(outcome.errors.join(" | "))
    }
}

/// Messages of the bill page, wrapped in [`Message::Bill`].
#[derive(Debug, Clone)]
pub enum BillMessage {
    Input(BillField, String),
    ToggleMonitor(bool),
    ToggleAutoTariff(bool),
    ToggleSettings,
    Save,
    ResetDefaults,
    RefreshTariff,
    TariffRefreshed(Result<String, String>),
    ExportMarkdown,
    CopyMarkdown,
    OpenReports,
}

fn bm(m: BillMessage) -> Message {
    Message::Bill(m)
}

// ---------------------------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------------------------

fn money(v: f64, language: AppLanguage) -> String {
    format!("R$ {}", format_number(v.max(0.0), 2, language))
}

fn card<'a>(content: impl Into<Element<'a, Message, AppTheme>>, style: ContainerStyle) -> Element<'a, Message, AppTheme> {
    Container::new(content)
        .width(Length::Fill)
        .padding(Padding::from(PADDING_LARGE))
        .class(style)
        .into()
}

fn title<'a>(text: impl Into<String>) -> Element<'a, Message, AppTheme> {
    Text::new(text.into())
        .size(FONT_SIZE_TITLE)
        .font(FONT_BOLD)
        .class(TextStyle::Subtitle)
        .into()
}

fn muted<'a>(text: impl Into<String>) -> Element<'a, Message, AppTheme> {
    Text::new(text.into()).size(FONT_SIZE_SMALL).class(TextStyle::Muted).into()
}

fn tile<'a>(label: &'a str, value: String, sub: String, style: TextStyle) -> Element<'a, Message, AppTheme> {
    Column::new()
        .width(Length::FillPortion(1))
        .spacing(2)
        .align_x(Alignment::Center)
        .push(Text::new(label).size(FONT_SIZE_BODY).font(FONT_BOLD).class(TextStyle::Subtitle))
        .push(Text::new(value).size(FONT_SIZE_SUBTITLE).font(FONT_BOLD).class(style))
        .push(Text::new(sub).size(FONT_SIZE_SMALL).class(TextStyle::Muted))
        .into()
}

pub fn view<'a>(state: &'a BillState, language: AppLanguage, pc_wall_watts: f64) -> Element<'a, Message, AppTheme> {
    let mut content = Column::new()
        .spacing(SPACING_XLARGE)
        .padding(Padding::from(PADDING_LARGE))
        .width(Length::Fill);

    match &state.report {
        Some(report) => {
            content = content
                .push(month_card(report, language))
                .push(now_card(state, report, language, pc_wall_watts))
                .push(tariff_card(state, report, language))
                .push(history_card(report, language));
        }
        None => {
            content = content.push(card(
                muted(t(language, "Sem dados ainda.", "No data yet.")),
                ContainerStyle::Card,
            ))
        }
    }
    content = content.push(actions_card(state, language));
    content = content.push(settings_card(state, language));

    Scrollable::new(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .class(ScrollableStyle::Standard)
        .into()
}

fn month_card<'a>(report: &'a BillingReport, language: AppLanguage) -> Element<'a, Message, AppTheme> {
    let Some(m) = report.current_month() else {
        return card(muted("-"), ContainerStyle::PowerCard);
    };
    let period = format!(
        "{} {} – {}",
        t(language, "Ciclo", "Cycle"),
        m.start.map(|d| d.format("%d/%m").to_string()).unwrap_or_default(),
        m.end.map(|d| d.format("%d/%m/%Y").to_string()).unwrap_or_default()
    );
    let head = Column::new()
        .width(Length::FillPortion(1))
        .spacing(SPACING_SMALL)
        .align_x(Alignment::Center)
        .push(
            Text::new(t(language, "Gasto do PC neste mês", "PC cost this month"))
                .size(FONT_SIZE_SUBTITLE)
                .font(FONT_BOLD)
                .class(TextStyle::Subtitle),
        )
        .push(
            Text::new(money(m.total_cost(), language))
                .size(FONT_SIZE_LARGE)
                .font(FONT_BOLD)
                .class(TextStyle::Primary),
        )
        .push(muted(period))
        .push(muted(match m.projection {
            Some(p) => format!(
                "{} {}",
                t(language, "Projeção do ciclo:", "Cycle projection:"),
                money(p, language)
            ),
            None => String::new(),
        }));

    let tiles = Row::new()
        .width(Length::FillPortion(2))
        .spacing(SPACING_MEDIUM)
        .push(tile(
            t(language, "PC (na tomada)", "PC (wall)"),
            format!("{} kWh", format_number(m.pc_wall_kwh, 2, language)),
            format!(
                "{} · {} h",
                money(m.cost_pc, language),
                format_number(m.pc_on_hours, 1, language)
            ),
            TextStyle::Secondary,
        ))
        .push(tile(
            t(language, "Monitor", "Monitor"),
            format!("{} kWh", format_number(m.monitor_kwh, 2, language)),
            format!(
                "{} · {} h {}",
                money(m.cost_monitor, language),
                format_number(m.monitor_on_hours, 1, language),
                t(language, "ligado", "on")
            ),
            TextStyle::Secondary,
        ))
        .push(tile(
            t(language, "Preço médio", "Average price"),
            format!("{}/kWh", money(m.avg_price(), language)),
            if m.flags.is_empty() {
                String::new()
            } else {
                format!("{} {}", t(language, "Bandeira", "Flag"), m.flags.join("/"))
            },
            TextStyle::Tertiary,
        ));

    card(
        Row::new()
            .spacing(SPACING_LARGE)
            .align_y(Alignment::Center)
            .push(head)
            .push(tiles),
        ContainerStyle::PowerCard,
    )
}

fn monitor_line(status: Option<&MonitorStatus>, language: AppLanguage) -> (String, String) {
    let Some(s) = status else {
        return (
            t(language, "Sem leitura do monitor ainda", "No monitor reading yet").to_string(),
            String::new(),
        );
    };
    let watts = format!("{} W", format_number(s.watts, 1, language));
    let main = match s.state {
        MonitorPowerState::On => match s.brightness {
            Some(b) => format!(
                "{} — {} · {} {}% · ~{}",
                s.name,
                t(language, "Ligado", "On"),
                t(language, "brilho", "brightness"),
                b,
                watts
            ),
            None => format!("{} — {} · ~{}", s.name, t(language, "Ligado", "On"), watts),
        },
        MonitorPowerState::Standby => format!("{} — {} · {}", s.name, t(language, "Em espera", "Standby"), watts),
        MonitorPowerState::Off => format!(
            "{} — {} · {} ({})",
            s.name,
            t(language, "Desligado", "Off"),
            watts,
            t(language, "consumo em standby", "standby draw")
        ),
        MonitorPowerState::NotDetected => {
            t(language, "Nenhum monitor físico detectado", "No physical monitor detected").to_string()
        }
    };
    let mut detail = Vec::new();
    if !s.virtual_names.is_empty() {
        detail.push(format!(
            "{} {}",
            t(language, "Tela virtual ativa (Moonlight/Sunshine):", "Virtual display active (Moonlight/Sunshine):"),
            s.virtual_names
        ));
    }
    if s.state == MonitorPowerState::On && !s.ddc_ok {
        detail.push(
            t(
                language,
                "DDC/CI sem resposta: usando o brilho padrão configurado.",
                "No DDC/CI answer: using the configured default brightness.",
            )
            .to_string(),
        );
    }
    let age_s = (chrono::Local::now().timestamp_millis() - s.updated_ms) / 1000;
    if age_s > 30 {
        detail.push(format!(
            "{} {} s",
            t(language, "Leitura parada há", "Reading stale for"),
            age_s
        ));
    }
    (main, detail.join(" · "))
}

fn now_card<'a>(
    state: &'a BillState,
    report: &'a BillingReport,
    language: AppLanguage,
    pc_wall_watts: f64,
) -> Element<'a, Message, AppTheme> {
    let extra = report.settings.extra_watts;
    let pc_now = pc_wall_watts + if pc_wall_watts > 0.0 { extra } else { 0.0 };
    let price = report.today_price.as_ref().map(|p| p.final_kwh).unwrap_or(0.0);
    let monitor_w = state
        .monitor
        .as_ref()
        .filter(|_| report.settings.monitor_enabled)
        .map(|m| m.watts)
        .unwrap_or(0.0);
    let per_hour = (pc_now + monitor_w) / 1000.0 * price;
    let (monitor_main, monitor_detail) = monitor_line(state.monitor.as_ref(), language);

    let mut col = Column::new()
        .spacing(SPACING_MEDIUM)
        .push(title(t(language, "Agora", "Right now")))
        .push(
            Text::new(format!(
                "PC: {} W {} · {}: {} {}",
                format_number(pc_now, 1, language),
                t(language, "na tomada", "at the wall"),
                t(language, "custo por hora", "cost per hour"),
                money(per_hour, language),
                t(language, "(PC + monitor)", "(PC + monitor)"),
            ))
            .size(FONT_SIZE_BODY),
        );
    if report.settings.monitor_enabled {
        col = col
            .push(Text::new(format!("{}: {monitor_main}", t(language, "Monitor", "Monitor"))).size(FONT_SIZE_BODY));
        if !monitor_detail.is_empty() {
            col = col.push(muted(monitor_detail));
        }
    } else {
        col = col.push(muted(t(language, "Monitor não contabilizado (desativado nos ajustes).", "Monitor not counted (disabled in settings).")));
    }
    card(col, ContainerStyle::Card)
}

fn tariff_card<'a>(state: &'a BillState, report: &'a BillingReport, language: AppLanguage) -> Element<'a, Message, AppTheme> {
    let s = &report.settings;
    let mut col = Column::new().spacing(SPACING_SMALL).push(title(format!(
        "{} — {} ({})",
        t(language, "Tarifa", "Tariff"),
        s.distributor,
        s.region_label
    )));
    match &report.today_price {
        Some(p) => {
            col = col
                .push(
                    Text::new(format!(
                        "{}/kWh {}",
                        money(p.final_kwh, language),
                        t(language, "com impostos", "with taxes")
                    ))
                    .size(FONT_SIZE_SUBTITLE)
                    .font(FONT_BOLD)
                    .class(TextStyle::Primary),
                )
                .push(Text::new(format!(
                    "TE + TUSD {} + {} {} {} · ICMS {}% + PIS/COFINS {}%",
                    format_number(p.base_kwh, 4, language),
                    t(language, "bandeira", "flag"),
                    p.flag,
                    format_number(p.flag_kwh, 4, language),
                    format_number(s.icms_pct, 1, language),
                    format_number(s.pis_cofins_pct, 2, language),
                ))
                .size(FONT_SIZE_BODY))
                .push(muted(format!(
                    "{}{}{}",
                    p.resolution,
                    if p.flag_provisional {
                        t(
                            language,
                            " · bandeira do mês ainda não publicada nos dados abertos: usando a última conhecida",
                            " · this month's flag is not in the open data yet: using the last known one",
                        )
                    } else {
                        ""
                    },
                    if p.period_estimated {
                        t(language, " · fora da vigência conhecida", " · outside the known validity period")
                    } else {
                        ""
                    }
                )));
        }
        None => {
            col = col.push(Text::new(t(language, "Tarifa indisponível.", "Tariff unavailable.")).size(FONT_SIZE_BODY));
        }
    }
    col = col.push(muted(format!(
        "{} {}{}",
        t(language, "Fonte:", "Source:"),
        report.tariff.origin,
        report
            .tariff
            .last_ok
            .as_ref()
            .map(|d| format!(" · {} {d}", t(language, "atualizado em", "updated")))
            .unwrap_or_default()
    )));
    if let Some(err) = &report.tariff.last_error {
        col = col.push(muted(format!("{} {err}", t(language, "Última falha:", "Last error:"))));
    }
    col = col.push(
        button(Text::new(if state.refreshing {
            t(language, "Atualizando…", "Updating…")
        } else {
            t(language, "Atualizar tarifa agora", "Update tariff now")
        }).size(FONT_SIZE_BODY))
        .class(ButtonStyle::Standard)
        .on_press_maybe((!state.refreshing).then_some(bm(BillMessage::RefreshTariff))),
    );
    card(col, ContainerStyle::Card)
}

fn cell<'a>(text: String, portion: u16, bold: bool, style: TextStyle) -> Element<'a, Message, AppTheme> {
    let mut txt = Text::new(text).size(FONT_SIZE_BODY).class(style);
    if bold {
        txt = txt.font(FONT_BOLD);
    }
    Container::new(txt).width(Length::FillPortion(portion)).into()
}

fn history_card<'a>(report: &'a BillingReport, language: AppLanguage) -> Element<'a, Message, AppTheme> {
    let header = Row::new()
        .spacing(SPACING_SMALL)
        .push(cell(t(language, "Mês", "Month").into(), 2, true, TextStyle::Subtitle))
        .push(cell(t(language, "PC ligado", "PC on").into(), 2, true, TextStyle::Subtitle))
        .push(cell("PC kWh".into(), 2, true, TextStyle::Subtitle))
        .push(cell(t(language, "Monitor kWh", "Monitor kWh").into(), 2, true, TextStyle::Subtitle))
        .push(cell("R$/kWh".into(), 2, true, TextStyle::Subtitle))
        .push(cell(t(language, "Bandeira", "Flag").into(), 2, true, TextStyle::Subtitle))
        .push(cell(t(language, "Total", "Total").into(), 2, true, TextStyle::Subtitle));
    let mut col = Column::new()
        .spacing(SPACING_SMALL)
        .push(title(t(language, "Histórico por mês", "Monthly history")))
        .push(header)
        .push(separator());
    for m in &report.months {
        col = col.push(
            Row::new()
                .spacing(SPACING_SMALL)
                .push(cell(
                    format!("{}{}", m.label, if m.is_current { " *" } else { "" }),
                    2,
                    m.is_current,
                    TextStyle::Default,
                ))
                .push(cell(format!("{} h", format_number(m.pc_on_hours, 1, language)), 2, false, TextStyle::Default))
                .push(cell(format_number(m.pc_wall_kwh, 2, language), 2, false, TextStyle::Default))
                .push(cell(
                    format!(
                        "{} ({} h)",
                        format_number(m.monitor_kwh, 2, language),
                        format_number(m.monitor_on_hours, 0, language)
                    ),
                    2,
                    false,
                    TextStyle::Default,
                ))
                .push(cell(format_number(m.avg_price(), 3, language), 2, false, TextStyle::Default))
                .push(cell(
                    if m.flags.is_empty() { "-".into() } else { m.flags.join("/") },
                    2,
                    false,
                    TextStyle::Default,
                ))
                .push(cell(money(m.total_cost(), language), 2, true, TextStyle::Primary)),
        );
    }
    col = col.push(muted(t(
        language,
        "* mês em andamento. Custo marginal do PC: não inclui iluminação pública (CIP) nem custo de disponibilidade.",
        "* current month. Marginal PC cost: excludes public lighting fee (CIP) and minimum availability charge.",
    )));
    card(col, ContainerStyle::Card)
}

fn actions_card<'a>(state: &'a BillState, language: AppLanguage) -> Element<'a, Message, AppTheme> {
    let btn = |label: &'static str, msg: BillMessage| {
        button(Text::new(label).size(FONT_SIZE_BODY))
            .class(ButtonStyle::Standard)
            .on_press(bm(msg))
    };
    let mut col = Column::new()
        .spacing(SPACING_MEDIUM)
        .push(title(t(language, "Relatório para a IA", "Report for AI")))
        .push(muted(t(
            language,
            "Resumo curto em Markdown (só meses, kWh, R$ e premissas) para colar numa IA sem gastar contexto.",
            "Short Markdown summary (months, kWh, cost and assumptions only) to paste into an AI chat.",
        )))
        .push(
            Row::new()
                .spacing(SPACING_MEDIUM)
                .push(btn(t(language, "Exportar .md", "Export .md"), BillMessage::ExportMarkdown))
                .push(btn(t(language, "Copiar .md", "Copy .md"), BillMessage::CopyMarkdown))
                .push(btn(t(language, "Abrir pasta de relatórios", "Open reports folder"), BillMessage::OpenReports))
                .wrap()
                .vertical_spacing(SPACING_MEDIUM),
        );
    if let Some(status) = &state.status {
        col = col.push(Text::new(status.clone()).size(FONT_SIZE_BODY).class(TextStyle::Tertiary));
    }
    card(col, ContainerStyle::Card)
}

fn input_row<'a>(
    label: &'a str,
    hint: &'a str,
    value: &'a str,
    field: BillField,
) -> Element<'a, Message, AppTheme> {
    Row::new()
        .spacing(SPACING_LARGE)
        .align_y(Alignment::Center)
        .push(Text::new(label).size(FONT_SIZE_BODY).width(Length::FillPortion(3)))
        .push(
            text_input(hint, value)
                .on_input(move |v| bm(BillMessage::Input(field, v)))
                .padding(PADDING_MEDIUM)
                .width(Length::FillPortion(2)),
        )
        .into()
}

fn toggle_row<'a>(label: &'a str, value: bool, f: fn(bool) -> BillMessage) -> Element<'a, Message, AppTheme> {
    Row::new()
        .spacing(SPACING_LARGE)
        .align_y(Alignment::Center)
        .push(Text::new(label).size(FONT_SIZE_BODY).width(Length::FillPortion(3)))
        .push(
            Container::new(
                toggler(value)
                    .on_toggle(move |v| bm(f(v)))
                    .class(TogglerStyle::Standard),
            )
            .width(Length::FillPortion(2)),
        )
        .into()
}

fn settings_card<'a>(state: &'a BillState, language: AppLanguage) -> Element<'a, Message, AppTheme> {
    let toggle = button(
        Text::new(if state.settings_open {
            t(language, "Ocultar ajustes do cálculo", "Hide calculation settings")
        } else {
            t(language, "Ajustes do cálculo", "Calculation settings")
        })
        .size(FONT_SIZE_BODY),
    )
    .class(ButtonStyle::Standard)
    .on_press(bm(BillMessage::ToggleSettings));
    if !state.settings_open {
        return card(Column::new().push(toggle), ContainerStyle::Card);
    }
    let i = &state.inputs;
    let col = Column::new()
        .spacing(SPACING_MEDIUM)
        .push(toggle)
        .push(title(t(language, "Tarifa e impostos", "Tariff and taxes")))
        .push(toggle_row(
            t(language, "Usar tarifa automática da ANEEL no painel", "Use the automatic ANEEL tariff on the dashboard"),
            i.use_auto_tariff,
            BillMessage::ToggleAutoTariff,
        ))
        .push(input_row(t(language, "Distribuidora (sigla ANEEL)", "Distributor (ANEEL code)"), "ENEL CE", &i.distributor, BillField::Distributor))
        .push(input_row(t(language, "Região (só exibição)", "Region (display only)"), "Crateús - CE", &i.region, BillField::Region))
        .push(input_row("ICMS (%)", "20", &i.icms, BillField::Icms))
        .push(input_row(t(language, "PIS + COFINS (%) — veja na sua conta", "PIS + COFINS (%) — see your bill"), "5,7", &i.pis_cofins, BillField::PisCofins))
        .push(input_row(t(language, "Dia em que o ciclo de leitura começa", "Day the meter-reading cycle starts"), "1", &i.billing_day, BillField::BillingDay))
        .push(input_row(t(language, "W extras com o PC ligado (placa-mãe, fans…)", "Extra W while the PC is on (motherboard, fans…)"), "0", &i.extra_watts, BillField::ExtraWatts))
        .push(muted(t(
            language,
            "A eficiência da fonte (PSU) fica na engrenagem de Configurações e também entra no cálculo.",
            "PSU efficiency is set in the gear Settings and is applied as well.",
        )))
        .push(separator())
        .push(title(t(language, "Monitor", "Monitor")))
        .push(toggle_row(t(language, "Contabilizar o monitor", "Count the monitor"), i.monitor_enabled, BillMessage::ToggleMonitor))
        .push(input_row(t(language, "W com brilho 0%", "W at 0% brightness"), "8", &i.monitor_min, BillField::MonitorMin))
        .push(input_row(t(language, "W com brilho 100%", "W at 100% brightness"), "34", &i.monitor_max, BillField::MonitorMax))
        .push(input_row(t(language, "W em espera / desligado", "W in standby / off"), "0,3", &i.monitor_standby, BillField::MonitorStandby))
        .push(input_row(t(language, "Brilho assumido sem DDC/CI (%)", "Brightness assumed without DDC/CI (%)"), "90", &i.monitor_default_brightness, BillField::MonitorDefaultBrightness))
        .push(input_row(t(language, "Filtrar monitor (ex.: AOCB402; vazio = todos)", "Monitor filter (e.g. AOCB402; empty = all)"), "", &i.monitor_match, BillField::MonitorMatch))
        .push(muted(t(
            language,
            "Padrão AOC 27G4: 8–34 W (manual: 31 W típico, ≤46 W no máximo com USB/áudio; etiqueta UE 24 W). Com um medidor de tomada você pode calibrar.",
            "AOC 27G4 default: 8–34 W (manual: 31 W typical, ≤46 W max incl. USB/audio; EU label 24 W). Calibrate with a plug meter if you have one.",
        )))
        .push(
            Row::new()
                .spacing(SPACING_MEDIUM)
                .push(
                    button(Text::new(t(language, "Salvar", "Save")).size(FONT_SIZE_BODY))
                        .class(ButtonStyle::Standard)
                        .on_press(bm(BillMessage::Save)),
                )
                .push(
                    button(Text::new(t(language, "Restaurar padrão", "Restore defaults")).size(FONT_SIZE_BODY))
                        .class(ButtonStyle::Standard)
                        .on_press(bm(BillMessage::ResetDefaults)),
                ),
        );
    card(col, ContainerStyle::Card)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_roundtrip_and_validation() {
        let defaults = BillingSettings::default();
        let inputs = BillInputs::from_settings(&defaults);
        assert_eq!(inputs.pis_cofins, "5,7");
        assert_eq!(inputs.to_settings().unwrap(), defaults);
        let mut bad = inputs.clone();
        bad.billing_day = "31".into();
        assert!(bad.to_settings().is_err());
        let mut bad = inputs;
        bad.monitor_max = "1".into();
        assert!(bad.to_settings().is_err());
    }
}
