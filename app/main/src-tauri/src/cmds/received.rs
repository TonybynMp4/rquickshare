use std::path::{Path, PathBuf};

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::AppState;

/// Extensions the OS runs, installs or interprets as code when they are
/// opened. Received files come from whoever is nearby, so "Open" shows these
/// in their folder rather than launching them; the user can still run one
/// deliberately from there.
const RUNNABLE_EXTENSIONS: &[&str] = &[
    // Windows
    "exe",
    "com",
    "scr",
    "pif",
    "cpl",
    "msi",
    "msp",
    "msix",
    "msixbundle",
    "appx",
    "appxbundle",
    "appinstaller",
    "application",
    "appref-ms",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "psd1",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "hta",
    "lnk",
    "url",
    "reg",
    "inf",
    "msc",
    "scf",
    "chm",
    "gadget",
    "jar",
    "settingcontent-ms",
    "library-ms",
    "search-ms",
    // Linux
    "desktop",
    "sh",
    "bash",
    "run",
    "appimage",
    "deb",
    "rpm",
    "flatpakref",
    "snap",
    // Anywhere an interpreter is associated
    "py",
    "pyw",
    "pl",
    "rb",
];

fn is_runnable(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        RUNNABLE_EXTENSIONS
            .iter()
            .any(|r| e.eq_ignore_ascii_case(r))
    })
}

/// The files a finished inbound transfer saved. Looked up by transfer id, so
/// the webview never hands us a path to open: it can only ask about files
/// this app itself wrote.
fn received_files(id: &str, state: &AppState) -> Result<Vec<PathBuf>, String> {
    state
        .received
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
        .filter(|files| !files.is_empty())
        .ok_or_else(|| format!("no received files for transfer {id}"))
}

/// "Open" on a received transfer: the file itself when there is one, the
/// folder when there are several, and the folder too for anything that
/// would run. Returns "opened" or "revealed", so the front end can say which.
#[tauri::command]
pub fn open_received(
    id: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<&'static str, String> {
    let files = received_files(&id, &state)?;
    let opener = app.opener();

    if let [file] = files.as_slice() {
        if is_runnable(file) {
            info!("open_received: {file:?} could run code, revealing it instead");
            opener.reveal_item_in_dir(file).map_err(|e| e.to_string())?;
            return Ok("revealed");
        }
        opener
            .open_path(file.to_string_lossy(), None::<&str>)
            .map_err(|e| e.to_string())?;
        return Ok("opened");
    }

    opener
        .reveal_items_in_dir(&files)
        .map_err(|e| e.to_string())?;
    Ok("revealed")
}

/// "Open folder" on a received transfer: its files, selected in the file
/// manager.
#[tauri::command]
pub fn reveal_received(
    id: String,
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let files = received_files(&id, &state)?;
    app.opener()
        .reveal_items_in_dir(&files)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runnable_files_are_recognized_whatever_the_case() {
        assert!(is_runnable(Path::new("setup.EXE")));
        assert!(is_runnable(Path::new("dir/run.Bat")));
        assert!(is_runnable(Path::new("launcher.desktop")));
        assert!(is_runnable(Path::new("tool.AppImage")));
        assert!(!is_runnable(Path::new("photo.jpg")));
        assert!(!is_runnable(Path::new("notes.txt")));
        assert!(!is_runnable(Path::new("README")));
    }
}
