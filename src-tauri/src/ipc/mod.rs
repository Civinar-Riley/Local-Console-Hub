//! IPC layer — Tauri commands, events and DTO mapping.
//!
//! This module is the explicit frontend/backend contract
//! (`MVP_IMPLEMENTATION_SPEC.md` §9): commands are named and typed, there is
//! no generic "execute arbitrary backend action" command, and every session
//! event includes the session id.
//!
//! Current surface: the `ping` bootstrap command proving the typed invoke path
//! end to end (T00), startup config diagnostics, the session lifecycle and
//! service-action commands (T04, T08), the logging read/action commands (T05),
//! the terminal commands (T07), the logs view's file and retention commands
//! (T10) and the launch request a shortcut made (#63).
//!
//! ## Only in-memory reads stay on the main thread
//!
//! A command without `async` runs on the main thread, which is the window's
//! event loop. That is fine for a snapshot or a config lookup, and wrong for
//! everything that waits: a stop that cannot make a tree unwind spends up to
//! ~20s in the stop ladder, a shell handoff waits for its window, and a
//! listener refresh reads the OS tables. `#[tauri::command(async)]` moves
//! those calls to Tauri's async runtime, so the window and the tray keep
//! rendering the `Stopping` state the call just published. Commands that only
//! read memory (`list_sessions`, `get_session`, the terminal attach/write/
//! resize trio) stay synchronous; terminal input in particular must keep
//! arriving in the order the user produced it.

// `generate_handler!` resolves each command through hidden items the macro
// emits beside the function, so commands are registered by their own module
// path (`ipc::session::start_session`) rather than re-exported here.
pub mod confirm_end;
pub mod discovery;
pub mod launch;
pub mod listen;
pub mod logs;
pub mod resources;
pub mod session;
pub mod terminal;

use serde::Serialize;

use crate::session::core::SessionError;
use crate::shell;

/// Report a failed shell handoff as the session operation it was.
///
/// The shell layer knows the target and the OS's answer; the frontend knows one
/// structured error shape per session operation, so the two are joined here
/// rather than by teaching the window about a second error type. Shared by every
/// command whose action ends in the OS opening something for a session — a log
/// file, a log folder, a URL, a working directory.
pub fn hand_over_failed(
    session_id: &str,
    operation: &str,
    error: shell::ShellError,
) -> SessionError {
    SessionError::failed(
        session_id,
        operation,
        format!("{} ({})", error.message, error.target),
        None,
    )
}

/// Version of the ping IPC contract.
///
/// Bump when the payload shape changes in a way the frontend must react to.
/// Keep in sync with `PING_PROTOCOL_VERSION` in `src/types/ipc.ts`.
pub const PING_PROTOCOL_VERSION: u8 = 1;

/// Application display name used across the IPC surface.
pub const APP_NAME: &str = "Local Console Hub";

/// Typed response of the `ping` command.
///
/// Serialized as camelCase to match the frontend type `PingResponse`
/// in `src/types/ipc.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingResponse {
    pub app_name: String,
    pub app_version: String,
    pub protocol: u8,
}

/// Bootstrap command (T00 acceptance: frontend can call a typed backend
/// ping command).
#[tauri::command]
pub fn ping() -> PingResponse {
    PingResponse {
        app_name: APP_NAME.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PING_PROTOCOL_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The payload fixture mirrored by `src/types/ipc.test.ts` — if this
    // contract changes, both sides must change together.

    #[test]
    fn ping_reports_app_identity_and_protocol() {
        let response = ping();
        assert_eq!(response.app_name, "Local Console Hub");
        assert_eq!(response.protocol, 1);
        assert!(!response.app_version.is_empty());
    }

    #[test]
    fn ping_serializes_to_the_camel_case_contract() {
        let value = serde_json::to_value(ping()).expect("ping response serializes");
        assert!(value.get("appName").is_some(), "missing appName in {value}");
        assert!(
            value.get("appVersion").is_some(),
            "missing appVersion in {value}"
        );
        assert!(
            value.get("protocol").is_some(),
            "missing protocol in {value}"
        );
        assert!(
            value.get("app_name").is_none(),
            "snake_case leaked into {value}"
        );
    }
}
