//! Single source of truth for where WattSeal stores its files.
//!
//! Fork: everything lives in one dedicated folder (Windows: `%LOCALAPPDATA%\WattSeal`):
//! - `power_monitoring.db` (+ SQLite `-wal`/`-shm`)
//! - `logs\collector.log`
//! - `relatorios\` (Markdown exports and the daily CSV backup)
//!
//! Files that older versions created in the folder the executable was started from are
//! migrated there safely (WAL checkpoint first, never while another collector holds them).
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const DB_FILE: &str = "power_monitoring.db";
const LOG_FILE: &str = "collector.log";
const REPORTS_DIR: &str = "relatorios";
// XDG convention is lowercase; Windows/macOS use the product name.
const APP_DIR: &str = if cfg!(target_os = "linux") {
    "wattseal"
} else {
    "WattSeal"
};

struct Locations {
    data: PathBuf,
    logs: PathBuf,
    custom: bool,
}

impl Locations {
    fn os_default() -> Self {
        let data = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join(APP_DIR);
        // `state_dir()` is Some only on Linux, elsewhere logs live in a `logs` subfolder of the data dir.
        let logs = dirs::state_dir()
            .map(|d| d.join(APP_DIR))
            .unwrap_or_else(|| data.join("logs"));
        Self {
            data,
            logs,
            custom: false,
        }
    }
}

static LOCATIONS: OnceLock<Locations> = OnceLock::new();

fn locations() -> &'static Locations {
    LOCATIONS.get_or_init(Locations::os_default)
}

/// Call once at startup, right after CLI parsing. A custom dir disables legacy migration.
pub fn init(custom_dir: Option<&PathBuf>) {
    let chosen = match custom_dir {
        Some(dir) => Locations {
            logs: dir.join("logs"),
            data: dir.to_path_buf(),
            custom: true,
        },
        None => Locations::os_default(),
    };
    let _ = LOCATIONS.set(chosen);
    let _ = fs::create_dir_all(data_dir());
    let _ = fs::create_dir_all(log_dir());
    let _ = fs::create_dir_all(reports_dir());
    if !locations().custom {
        migrate_legacy_files();
    }
}

pub fn data_dir() -> &'static Path {
    &locations().data
}
pub fn log_dir() -> &'static Path {
    &locations().logs
}
pub fn database_path() -> PathBuf {
    data_dir().join(DB_FILE)
}
pub fn log_file() -> PathBuf {
    log_dir().join(LOG_FILE)
}
/// Folder for Markdown reports and CSV backups.
pub fn reports_dir() -> PathBuf {
    data_dir().join(REPORTS_DIR)
}

/// `Some` only when `--data-dir` was given (so the UI child can be started with the same one).
pub fn custom_data_dir() -> Option<&'static Path> {
    locations().custom.then_some(data_dir())
}

/// Folders where older versions kept their files: the executable folder and the working directory.
fn legacy_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    if let Ok(cwd) = std::env::current_dir() {
        if !dirs.iter().any(|d| same_path(d, &cwd)) {
            dirs.push(cwd);
        }
    }
    dirs
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Moves files that older versions kept next to the executable (or in the working directory).
fn migrate_legacy_files() {
    let (data, logs) = (data_dir(), log_dir());

    // Logs written by builds that kept `collector.log` directly in the data dir.
    let flat_log = data.join(LOG_FILE);
    if logs != data && flat_log.exists() && !logs.join(LOG_FILE).exists() {
        move_file(&flat_log, &logs.join(LOG_FILE));
    }

    for old_dir in legacy_dirs() {
        if same_path(&old_dir, data) {
            continue;
        }
        migrate_from(&old_dir, data, logs);
    }
}

fn migrate_from(old_dir: &Path, data: &Path, logs: &Path) {
    let old_db = old_dir.join(DB_FILE);
    if old_db.exists() {
        if data.join(DB_FILE).exists() {
            crate::clog!(
                "⚠ Legacy database left untouched in {} (a database already exists in {})",
                old_dir.display(),
                data.display()
            );
            return;
        }
        if legacy_db_in_use(&old_db) {
            crate::clog!(
                "⚠ Legacy database in {} is in use by another WattSeal, migration postponed",
                old_dir.display()
            );
            return;
        }
        // Fold the WAL into the main file so nothing is lost when the side files are removed.
        if let Err(e) = checkpoint(&old_db) {
            crate::clog!("✗ Could not checkpoint legacy database, migration postponed: {e}");
            return;
        }
        if !move_file(&old_db, &data.join(DB_FILE)) {
            crate::clog!("✗ Could not move legacy database from {}", old_dir.display());
            return;
        }
        crate::clog!("✓ Database migrated from {} to {}", old_dir.display(), data.display());
        let _ = fs::remove_file(old_dir.join(format!("{DB_FILE}-wal")));
        let _ = fs::remove_file(old_dir.join(format!("{DB_FILE}-shm")));
        let _ = fs::remove_file(old_dir.join(format!("{DB_FILE}.collector.lock")));
    }

    let old_log = old_dir.join(LOG_FILE);
    if old_log.exists() && !logs.join(LOG_FILE).exists() {
        move_file(&old_log, &logs.join(LOG_FILE));
    }
}

/// True when another collector still holds the legacy lock file.
fn legacy_db_in_use(old_db: &Path) -> bool {
    use fs2::FileExt;
    let mut lock_path = old_db.as_os_str().to_owned();
    lock_path.push(".collector.lock");
    let lock_path = PathBuf::from(lock_path);
    if !lock_path.exists() {
        return false;
    }
    match fs::OpenOptions::new().write(true).open(&lock_path) {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                false
            }
            Err(_) => true,
        },
        Err(_) => true,
    }
}

fn checkpoint(db: &Path) -> Result<(), String> {
    let conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "busy_timeout", "5000")
        .map_err(|e| e.to_string())?;
    let busy: i64 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if busy != 0 {
        return Err("database busy".to_string());
    }
    // Switch the legacy file back to a rollback journal so it is one self-contained file.
    let _ = conn.pragma_update(None, "journal_mode", "DELETE");
    drop(conn);
    Ok(())
}

/// Moves `from` to `to` (rename, or copy + delete across volumes). Returns true on success.
fn move_file(from: &Path, to: &Path) -> bool {
    if !from.exists() {
        return false;
    }
    if fs::rename(from, to).is_ok() {
        return true;
    }
    if fs::copy(from, to).is_ok() {
        let _ = fs::remove_file(from);
        return true;
    }
    false
}
