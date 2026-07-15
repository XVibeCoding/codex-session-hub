pub mod core;
pub mod platform;
pub mod projection;
pub mod rollout;

use core::{
    BackupCleanupResult, BackupResult, BackupSummary, DesktopRefreshResult,
    ProjectionPreviewResult, RepairProgress, RepairResult, ScanResult, VerifyResult,
};
use platform::{BlockingProcess, CloseProcessResult, ProcessIdentity};
use projection::ProjectionScope;
use std::path::PathBuf;
use tauri::{ipc::Channel, Manager};

fn home() -> PathBuf {
    core::default_codex_home()
}

#[tauri::command]
async fn scan_codex() -> Result<ScanResult, String> {
    tauri::async_runtime::spawn_blocking(|| core::scan_at(&home()))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn refresh_desktop(
    selected_sources: Vec<String>,
    target_provider: String,
    observed_provider: String,
    initialize: bool,
) -> Result<DesktopRefreshResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        core::refresh_desktop_at(
            &home(),
            &selected_sources,
            &target_provider,
            &observed_provider,
            ProjectionScope::All,
            initialize,
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn create_backup() -> Result<BackupResult, String> {
    tauri::async_runtime::spawn_blocking(|| core::create_backup_safe_at(&home()))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn list_backups() -> Result<BackupSummary, String> {
    tauri::async_runtime::spawn_blocking(|| core::list_backups_at(&home()))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn cleanup_backups(include_legacy: bool) -> Result<BackupCleanupResult, String> {
    tauri::async_runtime::spawn_blocking(move || core::cleanup_backups_at(&home(), include_legacy))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn set_backup_retained(backup_path: String, retained: bool) -> Result<BackupSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        core::set_backup_pinned_at(&home(), std::path::Path::new(&backup_path), retained)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn open_backup_folder() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let codex_home = home();
        let path = core::backup_directory_at(&codex_home)?;
        platform::open_folder(&path)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn preview_projection(
    selected_sources: Vec<String>,
    target_provider: String,
    selected_thread_ids: Option<Vec<String>>,
) -> Result<ProjectionPreviewResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        core::preview_projection_selected_at(
            &home(),
            &selected_sources,
            &target_provider,
            ProjectionScope::All,
            selected_thread_ids.as_deref(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn repair_indexes(
    selected_sources: Vec<String>,
    target_provider: String,
    selected_thread_ids: Option<Vec<String>>,
    dry_run: bool,
    plan_token: Option<String>,
    on_progress: Channel<RepairProgress>,
) -> Result<RepairResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        core::repair_projection_selected_at_with_progress(
            &home(),
            &selected_sources,
            &target_provider,
            ProjectionScope::All,
            selected_thread_ids.as_deref(),
            dry_run,
            true,
            plan_token.as_deref(),
            move |event| {
                let _ = on_progress.send(event);
            },
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn verify_codex(
    selected_sources: Vec<String>,
    target_provider: String,
    selected_thread_ids: Option<Vec<String>>,
) -> Result<VerifyResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        core::verify_projection_selected_at(
            &home(),
            &selected_sources,
            &target_provider,
            ProjectionScope::All,
            selected_thread_ids.as_deref(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn rollback_latest() -> Result<VerifyResult, String> {
    tauri::async_runtime::spawn_blocking(|| core::restore_latest_at(&home()))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn restore_backup(backup_path: Option<String>) -> Result<VerifyResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let codex_home = home();
        let requested = backup_path.as_deref().map(std::path::Path::new);
        core::restore_backup_at(&codex_home, requested)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn list_blocking_processes() -> Result<Vec<BlockingProcess>, String> {
    tauri::async_runtime::spawn_blocking(|| platform::blocking_processes(&home()))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn close_blocking_process(
    identity: ProcessIdentity,
    force: bool,
) -> Result<CloseProcessResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        Ok(platform::close_process(&home(), &identity, force))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn reopen_codex(executable_path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        platform::reopen_codex(std::path::Path::new(&executable_path))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn open_project_folder(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || platform::open_folder(std::path::Path::new(&path)))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn reveal_rollout_file(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let codex_home = home();
        let requested = std::fs::canonicalize(&path)
            .map_err(|error| format!("cannot locate rollout file ({path}): {error}"))?;
        if !requested
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
        {
            return Err("refusing to reveal a non-JSONL session file".into());
        }
        let allowed = ["sessions", "archived_sessions"].iter().any(|directory| {
            std::fs::canonicalize(codex_home.join(directory))
                .map(|root| requested.starts_with(root))
                .unwrap_or(false)
        });
        if !allowed {
            return Err("rollout file is outside CODEX_HOME".into());
        }
        platform::reveal_file(&requested)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }));

    builder
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_codex,
            refresh_desktop,
            create_backup,
            list_backups,
            cleanup_backups,
            set_backup_retained,
            open_backup_folder,
            preview_projection,
            repair_indexes,
            verify_codex,
            rollback_latest,
            restore_backup,
            list_blocking_processes,
            close_blocking_process,
            reopen_codex,
            open_project_folder,
            reveal_rollout_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

pub fn run_cli() -> i32 {
    core::run_cli()
}
