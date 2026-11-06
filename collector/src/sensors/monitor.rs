//! Fork: external monitor power (AOC 27G4 by default) from DDC/CI brightness and display presence.
//!
//! How the state is inferred (Windows):
//! * The monitor is in the desktop (EnumDisplayMonitors) and DDC/CI answers → **On**, power from
//!   brightness (VCP 0x10). VCP 0xD6 (power mode) 2-5 → **Standby**.
//! * Windows switched the screens off (GUID_CONSOLE_DISPLAY_STATE = off) → **Standby**.
//! * It is still enumerated but stopped answering DDC/CI although it answered before → **Standby**
//!   (switched off with its button on a connection that keeps the EDID alive).
//! * It disappeared from the desktop (e.g. turned off, only the Sunshine/Moonlight virtual display
//!   "VDD by MTT" left) → **Off**, standby power only.
//! * Virtual displays never count as a monitor.
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use common::{EnergyUj, MonitorPowerState, MonitorReading, billing::MAX_ACCOUNTED_SAMPLE};

/// Latest inferred monitor situation, shared between the polling thread and the sampler.
#[derive(Debug, Clone)]
struct Snapshot {
    enabled: bool,
    name: String,
    state: MonitorPowerState,
    brightness: Option<u32>,
    watts: f64,
    virtual_displays: Vec<String>,
    ddc_ok: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            state: MonitorPowerState::NotDetected,
            brightness: None,
            watts: 0.0,
            virtual_displays: Vec::new(),
            ddc_ok: false,
        }
    }
}

/// Polls the monitor in a background thread (DDC/CI reads take ~50 ms each, so never in the sampler).
pub struct MonitorTracker {
    shared: Arc<Mutex<Snapshot>>,
}

const POLL_INTERVAL: Duration = Duration::from_secs(5);

impl MonitorTracker {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let thread_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("monitor-tracker".into())
            .spawn(move || platform::run(thread_shared));
        if let Err(e) = spawned {
            common::clog!("✗ Failed to start monitor tracker: {e}");
        }
        Self { shared }
    }

    /// Reading for a sample of length `dt` (capped so sleep/hibernate gaps are not billed).
    pub fn reading(&self, dt: Duration) -> Option<MonitorReading> {
        let snap = self.shared.lock().ok()?.clone();
        if !snap.enabled {
            return None;
        }
        let seconds = dt.min(MAX_ACCOUNTED_SAMPLE).as_secs_f64();
        Some(MonitorReading {
            name: snap.name,
            state: snap.state,
            brightness: snap.brightness,
            watts: snap.watts,
            energy: EnergyUj::from_joules(snap.watts * seconds),
            virtual_displays: snap.virtual_displays,
            ddc_ok: snap.ddc_ok,
        })
    }
}

/// A display currently part of the desktop.
#[derive(Debug, Clone)]
struct Probed {
    /// Stable key: hardware id (`MONITOR\AOCB402\...`) or the friendly name.
    key: String,
    name: String,
    is_virtual: bool,
    ddc: Option<Ddc>,
}

#[derive(Debug, Clone, Copy)]
struct Ddc {
    /// 0-100
    brightness: u32,
    /// VCP 0xD6: 1 on, 2-4 standby/suspend, 5 off.
    power_mode: Option<u32>,
}

/// Known virtual display drivers (Sunshine/Moonlight setups, Parsec, IddSample based drivers).
fn looks_virtual(hardware_id: &str, name: &str) -> bool {
    let id = hardware_id.to_uppercase();
    let n = name.to_lowercase();
    const IDS: [&str; 6] = ["MTT1337", "MSFT1337", "LNX0000", "PSC", "SMKD", "VDD"];
    const NAMES: [&str; 7] = ["vdd", "virtual", "iddsample", "parsec", "sunshine", "luminon", "dummy"];
    IDS.iter().any(|p| id.contains(&format!("\\{p}")))
        || NAMES.iter().any(|p| n.contains(p))
}

/// "MONITOR\AOCB402\{..}\0001" + "27G4" -> "AOC 27G4".
fn pretty_name(hardware_id: &str, friendly: &str) -> String {
    let vendor = hardware_id
        .split('\\')
        .nth(1)
        .map(|p| p.chars().take(3).collect::<String>())
        .filter(|v| v.len() == 3 && v.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_default();
    let friendly = friendly.trim();
    if vendor.is_empty() || friendly.to_uppercase().contains(&vendor) {
        friendly.to_string()
    } else if friendly.is_empty() {
        vendor
    } else {
        format!("{vendor} {friendly}")
    }
}

/// Pure state machine, unit-testable.
mod infer {
    use std::collections::HashMap;

    use common::{MonitorPowerState, billing::BillingSettings};

    use super::{Probed, Snapshot};

    /// Console display state from Windows: 0 off, 1 on, 2 dimmed.
    pub const CONSOLE_OFF: u32 = 0;

    #[derive(Default)]
    pub struct Memory {
        pub known_name: Option<String>,
        ddc_seen: HashMap<String, bool>,
        ddc_failures: HashMap<String, u32>,
        last_brightness: HashMap<String, u32>,
    }

    pub fn step(mem: &mut Memory, settings: &BillingSettings, displays: &[Probed], console: u32) -> Snapshot {
        let filter = settings.monitor_match.trim().to_lowercase();
        let virtual_displays: Vec<String> = displays.iter().filter(|d| d.is_virtual).map(|d| d.name.clone()).collect();
        let physical: Vec<&Probed> = displays
            .iter()
            .filter(|d| !d.is_virtual)
            .filter(|d| {
                filter.is_empty()
                    || d.key.to_lowercase().contains(&filter)
                    || d.name.to_lowercase().contains(&filter)
            })
            .collect();

        let mut any_on = false;
        let mut watts = 0.0;
        let mut brightness = None;
        let mut ddc_ok = false;
        let mut names = Vec::new();

        for d in &physical {
            names.push(d.name.clone());
            let mut on = console != CONSOLE_OFF;
            let b = match d.ddc {
                Some(ddc) => {
                    ddc_ok = true;
                    mem.ddc_seen.insert(d.key.clone(), true);
                    mem.ddc_failures.insert(d.key.clone(), 0);
                    mem.last_brightness.insert(d.key.clone(), ddc.brightness);
                    if matches!(ddc.power_mode, Some(2..=5)) {
                        on = false;
                    }
                    Some(ddc.brightness)
                }
                None => {
                    let fails = mem.ddc_failures.entry(d.key.clone()).or_insert(0);
                    *fails += 1;
                    if mem.ddc_seen.get(&d.key).copied().unwrap_or(false) && *fails >= 2 {
                        on = false; // answered before, silent now: switched off / asleep
                    }
                    mem.last_brightness.get(&d.key).copied()
                }
            };
            if on {
                any_on = true;
                let used = b.map(f64::from).unwrap_or(settings.monitor_default_brightness);
                watts += settings.monitor_watts(used);
                if brightness.is_none() {
                    brightness = b.or(Some(settings.monitor_default_brightness.round() as u32));
                }
            } else {
                watts += settings.monitor_w_standby;
            }
        }

        let state = if any_on {
            MonitorPowerState::On
        } else if !physical.is_empty() {
            MonitorPowerState::Standby
        } else if mem.known_name.is_some() {
            watts = settings.monitor_w_standby;
            MonitorPowerState::Off
        } else {
            MonitorPowerState::NotDetected
        };

        let name = if !names.is_empty() {
            names.join(" + ")
        } else {
            mem.known_name.clone().unwrap_or_default()
        };

        Snapshot {
            enabled: settings.monitor_enabled,
            name,
            state,
            brightness: if any_on { brightness } else { None },
            watts,
            virtual_displays,
            ddc_ok,
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::{
        ffi::c_void,
        mem,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use common::{
        Database,
        billing::{BillingSettings, kv_get, kv_set, load_settings},
    };
    use windows::{
        Win32::{
            Devices::Display::{
                DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
                GetVCPFeatureAndVCPFeatureReply, PHYSICAL_MONITOR,
            },
            Foundation::HANDLE,
            Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW, HMONITOR},
            System::Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING, PowerSettingRegisterNotification},
            UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK,
        },
        core::{GUID, PCWSTR},
    };

    use super::{Ddc, POLL_INTERVAL, Probed, Snapshot, infer, looks_virtual, pretty_name};

    const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
    const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
    /// 0 off, 1 on, 2 dimmed (Windows sends the current value right after registering).
    static CONSOLE_DISPLAY_STATE: AtomicU32 = AtomicU32::new(1);

    unsafe extern "system" fn power_callback(_context: *const c_void, kind: u32, setting: *const c_void) -> u32 {
        if kind == PBT_POWERSETTINGCHANGE && !setting.is_null() {
            let s = setting as *const POWERBROADCAST_SETTING;
            let (guid, len) = unsafe { ((*s).PowerSetting, (*s).DataLength) };
            if guid == GUID_CONSOLE_DISPLAY_STATE && len >= 4 {
                let data = unsafe { std::ptr::addr_of!((*s).Data) as *const u32 };
                let value = unsafe { data.read_unaligned() };
                CONSOLE_DISPLAY_STATE.store(value, Ordering::Relaxed);
            }
        }
        0
    }

    fn register_console_display_notification() {
        // Must outlive the registration: leaked on purpose (one per process).
        let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: std::ptr::null_mut(),
        }));
        let mut handle: *mut c_void = std::ptr::null_mut();
        let result = unsafe {
            PowerSettingRegisterNotification(
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void),
                &mut handle,
            )
        };
        if result.0 != 0 {
            common::clog!("⚠ Could not subscribe to display power notifications (error {})", result.0);
        }
    }

    fn wide_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    /// `\\.\DISPLAY1` -> `MONITOR\AOCB402\{...}\0001`
    fn monitor_hardware_id(device_name: &str) -> String {
        let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut dd = DISPLAY_DEVICEW {
            cb: mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut dd, 0) }.as_bool();
        if ok { wide_to_string(&dd.DeviceID) } else { String::new() }
    }

    fn read_ddc(hmonitor: HMONITOR) -> Option<Ddc> {
        unsafe {
            let mut count = 0u32;
            GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count).ok()?;
            if count == 0 {
                return None;
            }
            let mut monitors = vec![PHYSICAL_MONITOR::default(); count as usize];
            GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut monitors).ok()?;
            let handle = monitors[0].hPhysicalMonitor;
            let (mut current, mut maximum) = (0u32, 0u32);
            let ok = GetVCPFeatureAndVCPFeatureReply(handle, 0x10, None, &mut current, Some(&mut maximum as *mut u32)) != 0;
            let mut mode = 0u32;
            let mode_ok = ok && GetVCPFeatureAndVCPFeatureReply(handle, 0xD6, None, &mut mode, None) != 0;
            let _ = DestroyPhysicalMonitors(&monitors);
            if !ok {
                return None;
            }
            let brightness = if maximum > 0 {
                (current.min(maximum) * 100 + maximum / 2) / maximum
            } else {
                current.min(100)
            };
            Some(Ddc {
                brightness,
                power_mode: mode_ok.then_some(mode),
            })
        }
    }

    fn probe() -> Vec<Probed> {
        display_info::DisplayInfo::all()
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                let hardware_id = monitor_hardware_id(&d.name);
                let is_virtual = looks_virtual(&hardware_id, &d.friendly_name);
                let ddc = if is_virtual { None } else { read_ddc(d.raw_handle) };
                Probed {
                    key: if hardware_id.is_empty() {
                        d.friendly_name.clone()
                    } else {
                        hardware_id.clone()
                    },
                    name: pretty_name(&hardware_id, &d.friendly_name),
                    is_virtual,
                    ddc,
                }
            })
            .collect()
    }

    pub(super) fn run(shared: Arc<Mutex<Snapshot>>) {
        register_console_display_notification();
        let mut memory = infer::Memory::default();
        let mut settings = BillingSettings::default();
        let mut settings_loaded: Option<Instant> = None;
        let mut logged_state = None;

        loop {
            if settings_loaded.map_or(true, |t| t.elapsed() > Duration::from_secs(60)) {
                if let Ok(db) = Database::open_without_migrations() {
                    settings = load_settings(db.connection());
                    if memory.known_name.is_none() {
                        memory.known_name = kv_get(db.connection(), "monitor_known_name").filter(|n| !n.is_empty());
                    }
                }
                settings_loaded = Some(Instant::now());
            }

            let displays = if settings.monitor_enabled { probe() } else { Vec::new() };
            let console = CONSOLE_DISPLAY_STATE.load(Ordering::Relaxed);
            let snapshot = infer::step(&mut memory, &settings, &displays, console);

            // Remember the physical monitor so that "switched off" can be told apart from "never had one".
            if matches!(snapshot.state, common::MonitorPowerState::On | common::MonitorPowerState::Standby)
                && memory.known_name.as_deref() != Some(snapshot.name.as_str())
                && !snapshot.name.is_empty()
            {
                memory.known_name = Some(snapshot.name.clone());
                if let Ok(db) = Database::open_without_migrations() {
                    let _ = kv_set(db.connection(), "monitor_known_name", &snapshot.name);
                }
            }

            if logged_state != Some(snapshot.state) {
                common::clog!(
                    "Monitor: {} -> {:?} (brightness {:?}, {:.1} W, virtual: [{}])",
                    snapshot.name,
                    snapshot.state,
                    snapshot.brightness,
                    snapshot.watts,
                    snapshot.virtual_displays.join(", ")
                );
                logged_state = Some(snapshot.state);
            }

            if let Ok(mut guard) = shared.lock() {
                *guard = snapshot;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use std::sync::{Arc, Mutex};

    use super::Snapshot;

    /// DDC/CI is only implemented on Windows; elsewhere the monitor is simply not tracked.
    pub(super) fn run(shared: Arc<Mutex<Snapshot>>) {
        if let Ok(mut guard) = shared.lock() {
            guard.enabled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use common::{MonitorPowerState, billing::BillingSettings};

    use super::{Ddc, Probed, infer, looks_virtual, pretty_name};

    fn aoc(ddc: Option<Ddc>) -> Probed {
        Probed {
            key: "MONITOR\\AOCB402\\{4d36e96e}\\0001".into(),
            name: "AOC 27G4".into(),
            is_virtual: false,
            ddc,
        }
    }

    fn vdd() -> Probed {
        Probed {
            key: "MONITOR\\MTT1337\\{4d36e96e}\\0002".into(),
            name: "VDD by MTT".into(),
            is_virtual: true,
            ddc: None,
        }
    }

    #[test]
    fn names_and_virtual_detection() {
        assert_eq!(pretty_name("MONITOR\\AOCB402\\{x}\\0001", "27G4"), "AOC 27G4");
        assert_eq!(pretty_name("", "27G4"), "27G4");
        assert!(looks_virtual("MONITOR\\MTT1337\\{x}\\0001", "VDD by MTT"));
        assert!(!looks_virtual("MONITOR\\AOCB402\\{x}\\0001", "27G4"));
    }

    #[test]
    fn on_with_brightness() {
        let s = BillingSettings::default();
        let mut m = infer::Memory::default();
        let snap = infer::step(&mut m, &s, &[aoc(Some(Ddc { brightness: 80, power_mode: Some(1) }))], 1);
        assert_eq!(snap.state, MonitorPowerState::On);
        assert_eq!(snap.brightness, Some(80));
        assert!((snap.watts - s.monitor_watts(80.0)).abs() < 1e-9);
    }

    #[test]
    fn only_virtual_display_means_off_after_monitor_was_seen() {
        let s = BillingSettings::default();
        let mut m = infer::Memory::default();
        // never seen a physical monitor
        let snap = infer::step(&mut m, &s, &[vdd()], 1);
        assert_eq!(snap.state, MonitorPowerState::NotDetected);
        assert_eq!(snap.watts, 0.0);
        assert_eq!(snap.virtual_displays, vec!["VDD by MTT".to_string()]);
        m.known_name = Some("AOC 27G4".into());
        let snap = infer::step(&mut m, &s, &[vdd()], 1);
        assert_eq!(snap.state, MonitorPowerState::Off);
        assert!((snap.watts - s.monitor_w_standby).abs() < 1e-9);
    }

    #[test]
    fn silent_ddc_after_answering_means_standby() {
        let s = BillingSettings::default();
        let mut m = infer::Memory::default();
        infer::step(&mut m, &s, &[aoc(Some(Ddc { brightness: 50, power_mode: Some(1) }))], 1);
        let first_miss = infer::step(&mut m, &s, &[aoc(None)], 1);
        assert_eq!(first_miss.state, MonitorPowerState::On, "a single failed read is tolerated");
        let second_miss = infer::step(&mut m, &s, &[aoc(None)], 1);
        assert_eq!(second_miss.state, MonitorPowerState::Standby);
    }

    #[test]
    fn windows_screen_off_and_ddc_standby() {
        let s = BillingSettings::default();
        let mut m = infer::Memory::default();
        let snap = infer::step(&mut m, &s, &[aoc(Some(Ddc { brightness: 80, power_mode: Some(1) }))], 0);
        assert_eq!(snap.state, MonitorPowerState::Standby);
        let snap = infer::step(&mut m, &s, &[aoc(Some(Ddc { brightness: 80, power_mode: Some(4) }))], 1);
        assert_eq!(snap.state, MonitorPowerState::Standby);
    }

    #[test]
    fn monitor_without_ddc_uses_default_brightness() {
        let s = BillingSettings::default();
        let mut m = infer::Memory::default();
        let snap = infer::step(&mut m, &s, &[aoc(None)], 1);
        assert_eq!(snap.state, MonitorPowerState::On);
        assert!((snap.watts - s.monitor_watts(s.monitor_default_brightness)).abs() < 1e-9);
    }
}
