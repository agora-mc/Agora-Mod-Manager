pub use agora_core::crash_export::CrashReportContext;

/// Build a shareable crash report for `context`, including the instance's mod
/// list when the instance is known.
pub fn build_crash_report(app: &tauri::AppHandle, context: &CrashReportContext) -> String {
    match crate::paths::app_paths(app) {
        Ok(paths) => agora_core::crash_export::build_instance_crash_report(&paths, context),
        // No data folder means no manifest to read: the report goes without the mod list.
        Err(_) => agora_core::crash_export::build_crash_report(None, context),
    }
}
