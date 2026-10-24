//! Small local error log. It records app failures, never game assets or telemetry.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::Manager;

const MAX_LOG_BYTES: u64 = 1024 * 1024;
static LOG_WRITE: Mutex<()> = Mutex::new(());

pub fn path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_cache_dir()
        .map(|root| root.join("logs/viewer.log"))
        .map_err(|error| error.to_string())
}

/// Logging must never turn a recoverable preview failure into another UI error.
pub fn record(app: &tauri::AppHandle, kind: &str, message: &str) {
    let _ = write(app, kind, message);
}

fn write(app: &tauri::AppHandle, kind: &str, message: &str) -> Result<(), String> {
    let _guard = LOG_WRITE.lock().map_err(|error| error.to_string())?;
    let path = path(app)?;
    let folder = path.parent().ok_or("Missing log folder")?;
    fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    // At most the current log and one previous log are retained.
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let previous = folder.join("viewer.previous.log");
        if previous.is_file() {
            fs::remove_file(&previous).map_err(|error| error.to_string())?;
        }
        fs::rename(&path, previous).map_err(|error| error.to_string())?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let kind = kind
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(32)
        .collect::<String>();
    let message = message
        .chars()
        .take(4096)
        .map(|character| {
            if character == '\r' || character == '\n' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    writeln!(file, "{timestamp}\t{kind}\t{message}").map_err(|error| error.to_string())
}
