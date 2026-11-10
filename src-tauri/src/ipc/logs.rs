//! Logging commands (`docs/MVP_IMPLEMENTATION_SPEC.md` §9).
//!
//! The read side of T05's logging core: what a session's policy is doing
//! (`get_log_info`), what it has run (`get_run_history`), and the two actions
//! a user can take on a live run — save it (`save_run_log`, the `on_error`
//! policy's "I want this one after all") and switch recording on or off
//! (`set_log_recording`, the `manual` policy).
//!
//! T10 adds the logs *view*'s four actions (`docs/LOGGING.md` §10): open the
//! log file, open the folder that holds it, and the two halves of retention —
//! what a sweep would remove (`preview_log_cleanup`) and the sweep itself
//! (`cleanup_logs`). Like the lifecycle commands, each names a single Core
//! operation and adds nothing to it, and none of them decides anything the
//! view could have decided for itself.
//!
//! ## Why opening a file is a command rather than a path argument
//!
//! `open_log_file` takes a session id and an optional run id, never a path.
//! Session Core resolves those to a file the session actually owns, so the
//! webview cannot ask the OS to open an arbitrary path — the command's shape is
//! what makes that impossible, not a check inside it
//! (`crate::shell`'s module note).

use tauri::State;

use crate::logging::{CleanupReport, LogStatus, RunHistory};
use crate::session::core::{SessionCore, SessionError};
use crate::shell;

use super::hand_over_failed;

/// What this session's logging is doing, and where it writes
/// (`docs/LOGGING.md` §1.4).
#[tauri::command(async)]
pub fn get_log_info(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<LogStatus, SessionError> {
    core.log_status(&session_id)
        .ok_or_else(|| SessionError::unknown_session(&session_id, "get_log_info"))
}

/// A session's completed runs, newest first (`docs/LOGGING.md` §6).
///
/// A session with no history answers with an empty history rather than an
/// error: never having run is the normal state of a session that was just
/// configured.
///
/// Each entry also says whether the log it names is still on disk. Retention
/// sweeps log files and never the record of the run that wrote them
/// (`docs/LOGGING.md` §9), so a row has to be able to say "this run's log was
/// cleaned up" rather than offering an action that fails.
#[tauri::command(async)]
pub fn get_run_history(core: State<'_, SessionCore>, session_id: String) -> RunHistory {
    core.run_history(&session_id)
}

/// Commit the current run's `on_error` log now, instead of waiting to see how
/// the run ends (`docs/LOGGING.md` §3).
#[tauri::command(async)]
pub fn save_run_log(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<LogStatus, SessionError> {
    core.save_run_log(&session_id)
}

/// Turn recording on or off for a `manual` session's current run
/// (`docs/LOGGING.md` §3).
#[tauri::command(async)]
pub fn set_log_recording(
    core: State<'_, SessionCore>,
    session_id: String,
    recording: bool,
) -> Result<LogStatus, SessionError> {
    core.set_log_recording(&session_id, recording)
}

/// Open the log file this session's logging owns
/// (`docs/LOGGING.md` §10).
///
/// `run_id` names one entry of the run history; without it the session's
/// current file is opened — what it is writing now, or what its last run left
/// behind. A session that persists nothing answers with the reason its policy
/// gives rather than with an empty success.
#[tauri::command(async)]
pub fn open_log_file(
    core: State<'_, SessionCore>,
    session_id: String,
    run_id: Option<String>,
) -> Result<(), SessionError> {
    let path = core.log_file_path(&session_id, run_id.as_deref())?;
    shell::open_path(&path).map_err(|error| hand_over_failed(&session_id, "open_log_file", error))
}

/// Open the folder that holds this session's log file
/// (`docs/LOGGING.md` §10).
///
/// Deliberately the *file's* folder rather than the session's log root: an
/// `external` session's log belongs to the application (`docs/DECISIONS.md`
/// D-005), and sending a user to the Hub's own directory to look for it would
/// be showing them an empty folder as if it were the answer.
#[tauri::command(async)]
pub fn open_log_folder(
    core: State<'_, SessionCore>,
    session_id: String,
    run_id: Option<String>,
) -> Result<(), SessionError> {
    let path = core.log_folder_path(&session_id, run_id.as_deref())?;
    shell::open_path(&path).map_err(|error| hand_over_failed(&session_id, "open_log_folder", error))
}

/// What `cleanup_logs` would remove, without removing anything
/// (`docs/LOGGING.md` §10).
///
/// The confirmation step: a destructive action says how many files and how many
/// bytes it would take before the user agrees to it. Answers with an empty
/// report for a session with nothing past keeping, so "nothing to clean" is not
/// a failure.
#[tauri::command(async)]
pub fn preview_log_cleanup(core: State<'_, SessionCore>, session_id: String) -> CleanupReport {
    core.cleanup_preview(Some(&session_id))
}

/// Remove the Hub's own log files that retention says are past keeping for one
/// session (`docs/LOGGING.md` §9).
///
/// Only files under the Hub's logs root for *this* session are candidates: an
/// application-owned log linked with `source: external` lives elsewhere and is
/// never touched, and neither is another session's history. What was removed,
/// how much was freed, and what the OS refused all come back in the report.
#[tauri::command(async)]
pub fn cleanup_logs(core: State<'_, SessionCore>, session_id: String) -> CleanupReport {
    core.cleanup_logs(Some(&session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EffectiveLogMode, LogSource};
    use crate::logging::{BufferSummary, LogState, LogStatus, RunHistoryEntry};
    use crate::session::runtime::{RunId, RunRecord, Timestamp};

    // The payload fixtures mirrored by `src/types/logs.ts` — if a field name
    // changes on this side, the frontend's guards start rejecting the value and
    // both files have to change together.

    fn status() -> LogStatus {
        LogStatus {
            session_id: "comfyui".to_owned(),
            mode: EffectiveLogMode::Always,
            source: LogSource::Captured,
            state: LogState::Capturing,
            log_file: Some("C:/logs/comfyui/2026-09/run.log".to_owned()),
            log_file_present: true,
            external_log: None,
            session_log_dir: Some("C:/logs/comfyui/2026-09".to_owned()),
            records_input: false,
            buffer: BufferSummary {
                bytes: 24,
                lines: 3,
                dropped_bytes: 0,
            },
            truncated: false,
            last_error: None,
        }
    }

    #[test]
    fn the_log_status_payload_is_the_camel_case_contract() {
        let value = serde_json::to_value(status()).expect("the status serializes");

        for key in [
            "sessionId",
            "mode",
            "source",
            "state",
            "logFile",
            "logFilePresent",
            "sessionLogDir",
            "recordsInput",
            "buffer",
            "truncated",
        ] {
            assert!(value.get(key).is_some(), "missing {key} in {value}");
        }
        assert!(
            value.get("log_file").is_none(),
            "snake_case leaked into {value}"
        );
        // The file answer travels as a boolean and not as the path's shadow:
        // the frontend reads "logFile names a file **and** this is true", the
        // same rule a run-history entry is read by (D-022).
        assert_eq!(
            value["logFilePresent"],
            serde_json::json!(true),
            "the status's file answer must be a boolean: {value}"
        );
        // An absent optional arrives as `null`, not as a missing key: these
        // structs carry `Option` fields without `skip_serializing_if`, unlike
        // the config DTOs. Pinned here because the frontend mirror has to spell
        // "nothing here" the same way (`src/types/logs.ts`).
        assert_eq!(value["externalLog"], serde_json::Value::Null, "{value}");
        assert_eq!(value["lastError"], serde_json::Value::Null, "{value}");
    }

    /// One run record as the history carries it, so the history's own payload
    /// is pinned and not only the status's.
    ///
    /// The entry is the record *flattened* plus one key, not a record nested
    /// under a new one: the field names the frontend already reads have to keep
    /// the shape they have everywhere else (D-016).
    #[test]
    fn the_run_history_payload_is_the_camel_case_contract() {
        let history = RunHistory {
            runs: vec![RunHistoryEntry {
                run: RunRecord {
                    run_id: RunId::mint(),
                    session_id: "comfyui".to_owned(),
                    started_at: Timestamp::now(),
                    ended_at: None,
                    exit_code: None,
                    pid: Some(19002),
                    log_mode: EffectiveLogMode::Always,
                    log_source: LogSource::Captured,
                    log_file: None,
                },
                log_file_present: false,
            }],
            unreadable: Vec::new(),
        };

        let value = serde_json::to_value(history).expect("the history serializes");
        let run = &value["runs"][0];

        for key in [
            "runId",
            "sessionId",
            "startedAt",
            "logMode",
            "logSource",
            "logFilePresent",
        ] {
            assert!(run.get(key).is_some(), "missing {key} in {run}");
        }
        assert_eq!(
            run["logFilePresent"],
            serde_json::json!(false),
            "the file answer must not travel as a string: {run}"
        );
        assert!(
            run.get("run").is_none(),
            "the entry nested the record: {run}"
        );
        assert!(value.get("unreadable").is_some(), "{value}");
    }

    #[test]
    fn a_cleanup_report_serializes_what_the_view_reads() {
        let report = CleanupReport {
            removed: vec![std::path::PathBuf::from("C:/logs/svc/2026-09/run.log")],
            freed_bytes: 2048,
            failures: Vec::new(),
        };

        let value = serde_json::to_value(report).expect("the report serializes");

        assert_eq!(value["freedBytes"], serde_json::json!(2048));
        assert_eq!(
            value["removed"],
            serde_json::json!(["C:/logs/svc/2026-09/run.log"]),
            "a path must cross as a string the view can show"
        );
        assert!(value.get("failures").is_some(), "{value}");
    }
}
