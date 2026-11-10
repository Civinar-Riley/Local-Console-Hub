//! Session lifecycle commands (`docs/MVP_IMPLEMENTATION_SPEC.md` §9).
//!
//! Each command is a thin, named entry point onto Session Core — no logic
//! lives here beyond naming the operation. That is deliberate: the window and
//! the tray are meant to call the same Core APIs (§3), so a command must not
//! be able to do something the tray could not.
//!
//! Commands return the post-operation [`SessionRuntime`] so a caller has the
//! new state without waiting for the event it will also receive; a refusal or
//! a failure returns the structured [`SessionError`], which names the
//! operation and the reason (`docs/DEVELOPMENT.md` §9).
//!
//! Tauri maps the frontend's camelCase arguments onto these snake_case
//! parameters, so the frontend calls `startSession({ sessionId })`.

use serde::Serialize;
use tauri::State;

// Aliased so the commands below can keep the plain names the window calls:
// `add_application` and `save_terminal_config` are each a Tauri command *and*
// an app-layer operation, and the two are the same thing one call apart.
use crate::app::applications::{
    add_application as register_application, update_application as update_saved_application,
    NewApplication,
};
use crate::app::form::FormError;
use crate::app::terminals::{save_terminal_config as save_terminal, SaveTerminal};
use crate::config::{ConfigReportDto, SessionConfigDto};
use crate::session::core::{CreatedSession, SessionCore, SessionEntry, SessionError};
use crate::session::runtime::SessionRuntime;
use crate::shell;

use super::hand_over_failed;

/// What "新建 PowerShell" answers with (#62).
///
/// Both halves of the session it made, from the same operation that made it:
/// the window selects the new terminal by id and renders it from this pair, so
/// nothing has to follow the answer with a list read that may not have caught
/// up (spec #59 decision 4).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedSessionDto {
    pub config: SessionConfigDto,
    pub runtime: SessionRuntime,
}

impl CreatedSessionDto {
    /// A session that was created from the window (#62): removable once it has
    /// ended, and never restored after an exit.
    pub fn temporary(created: CreatedSession) -> Self {
        CreatedSessionDto {
            config: SessionConfigDto::from(&created.config).temporary(),
            runtime: created.runtime,
        }
    }

    /// A session that was saved into the config file (#64): listed now,
    /// reloaded by the next start, and not removable from the window.
    ///
    /// The pair is the same shape as a temporary one's — it is the same fact,
    /// "this session was just created, here are both halves" — and the flag is
    /// what differs. Naming the two constructors rather than writing the flag
    /// at each call site is what keeps that difference from being a detail a
    /// reader has to check.
    pub fn configured(created: CreatedSession) -> Self {
        CreatedSessionDto {
            config: SessionConfigDto::from(&created.config),
            runtime: created.runtime,
        }
    }
}

/// The startup config-loading and validation report. The report is captured
/// once during bootstrap; this read-only command never edits or reloads the
/// user's config file.
#[tauri::command]
pub fn get_config_report(report: State<'_, ConfigReportDto>) -> ConfigReportDto {
    report.inner().clone()
}

/// Every session's snapshot, in id order.
#[tauri::command]
pub fn list_sessions(core: State<'_, SessionCore>) -> Vec<SessionRuntime> {
    core.snapshots()
}

/// Every registered session's validated configuration, in id order.
///
/// The window renders a session from two halves: what it *is* — name, type,
/// purpose, close impact, port, cwd, shell — and what it is *doing* (the
/// snapshot above). This is the first half.
///
/// It answers from the same registry the snapshots come from, which is what
/// keeps the two halves describing one set of sessions: a row the window can
/// render is a session Session Core can start, stop and hand a terminal to.
///
/// Since #62 the list also says which rows are temporary — created from the
/// window rather than loaded from the config file — because that is the one
/// difference a row's controls act on (it is the row that can be removed).
#[tauri::command]
pub fn list_session_configs(core: State<'_, SessionCore>) -> Vec<SessionConfigDto> {
    core.entries()
        .iter()
        .map(SessionEntry::config_dto)
        .collect()
}

/// Create, register and start a temporary interactive terminal (#62).
///
/// The quick entry behind "新建 PowerShell": no form, no config file, and no
/// second kind of session — what it makes is a terminal session Session Core
/// starts, watches and closes the same way it does every other one. `cwd` is
/// the directory an external entry asked for; the session layer decides what
/// an absent one means (the user's home directory) rather than this thin
/// command doing it (spec #59 decision 5).
#[tauri::command(async)]
pub fn create_temporary_terminal(
    core: State<'_, SessionCore>,
    cwd: Option<String>,
) -> Result<CreatedSessionDto, SessionError> {
    core.create_temporary_terminal(cwd.as_deref())
        .map(CreatedSessionDto::temporary)
}

/// Save a new application and add it to the session list (#64).
///
/// The secondary entry behind "添加应用": a form's fields in, one configured
/// session out — saved to the user's config file (so the next start loads it)
/// and registered now (so this window lists it immediately). The two halves
/// are one operation because a row without a config entry, or a config entry
/// without a row, is the phantom the spec forbids.
///
/// The failure carries the form field it belongs to when there is one, so the
/// dialog can put the message beside the input rather than only in a banner.
///
/// The file to write is taken from the startup report, which is where the app
/// records the path it actually read — not from `AppPaths::from_env()` again,
/// which could resolve differently from the file the registry was built from.
#[tauri::command(async)]
pub fn add_application(
    core: State<'_, SessionCore>,
    report: State<'_, ConfigReportDto>,
    form: NewApplication,
) -> Result<CreatedSessionDto, FormError> {
    let config_file = report.config_path.as_deref().map(std::path::Path::new);
    register_application(&core, config_file, form).map(CreatedSessionDto::configured)
}

/// Edit a configured application's launch configuration.
///
/// The id is the session being edited, never one the form can change: an edit
/// changes what the entry runs, and the id keys the registry, the config file
/// and the session's log directory. The answer is the same pair an addition
/// answers with, so the window can re-render the row from the write that just
/// landed.
///
/// The file to write comes from the startup report for the same reason
/// [`add_application`] does.
#[tauri::command(async)]
pub fn update_application(
    core: State<'_, SessionCore>,
    report: State<'_, ConfigReportDto>,
    session_id: String,
    form: NewApplication,
) -> Result<CreatedSessionDto, FormError> {
    let config_file = report.config_path.as_deref().map(std::path::Path::new);
    update_saved_application(&core, config_file, &session_id, form)
        .map(CreatedSessionDto::configured)
}

/// What the Hub can confirm about a launch method's display mode (#66).
///
/// The "添加应用" form calls this as the user fills in the command, so the
/// recommendation is about the method that would actually run rather than about
/// the name typed above it (spec #59 decision 9). It is a read-only question:
/// nothing is resolved to a *running* program, and a launch method this build
/// cannot confirm comes back unrecommended with the reason why.
#[tauri::command(async)]
pub fn recommend_display(command: String, cwd: Option<String>) -> DisplayAdviceDto {
    DisplayAdviceDto::from(crate::app::recommend::advise(&command, cwd.as_deref()))
}

/// The recommendation as the form reads it (#66).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayAdviceDto {
    /// `"window"` when this build confirmed the method brings its own console,
    /// absent when it could not confirm either mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended: Option<String>,
    pub reason: String,
    /// The executable the command resolved to, when it resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
}

impl From<crate::app::recommend::DisplayAdvice> for DisplayAdviceDto {
    fn from(advice: crate::app::recommend::DisplayAdvice) -> Self {
        DisplayAdviceDto {
            recommended: advice.recommended.map(|mode| mode.as_str().to_owned()),
            reason: advice.reason,
            program: advice.program,
        }
    }
}

/// Save a running (or ended) temporary terminal's launch configuration (#65).
///
/// The other half of the quick entry: "新建 PowerShell" makes a terminal that no
/// file describes, and this makes one the config file does — the shell, the
/// directory and the name, and nothing about what was typed into it. The
/// session is not restarted, copied or re-keyed; it becomes a configured one
/// where it stands (`app::terminals` says why).
///
/// The answer is the same pair the quick entry answers with, so the window can
/// re-render the row it just saved from the answer rather than re-reading a
/// list that may not have caught up — and the runtime in it is the run that was
/// already going.
///
/// The file to write comes from the startup report for the same reason
/// [`add_application`] does: it records the file this process actually read.
#[tauri::command(async)]
pub fn save_terminal_config(
    core: State<'_, SessionCore>,
    report: State<'_, ConfigReportDto>,
    session_id: String,
    form: SaveTerminal,
) -> Result<CreatedSessionDto, FormError> {
    let config_file = report.config_path.as_deref().map(std::path::Path::new);
    save_terminal(&core, config_file, &session_id, form).map(CreatedSessionDto::configured)
}

/// Open a session: start it, or select the run it already has (#64).
///
/// Every "open this application" entry goes through this one operation
/// (spec #59 decision 10), so a second click on a running application cannot
/// start a second one, a click while it is starting cannot start another, and
/// a click while it is stopping is refused rather than queued behind the stop
/// barrier.
///
/// Since #66 it also answers what happened to the application's *own* window,
/// for the entries that keep one. That half belongs to the app layer
/// ([`crate::app::activation`]) because it is about a window rather than about
/// a lifecycle, and the same composition answers a launch request handed to the
/// Hub from a shortcut, so the two entries cannot disagree about what "open"
/// means.
#[tauri::command(async)]
pub fn activate_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<ActivationDto, SessionError> {
    crate::app::activation::open(&core, &session_id).map(ActivationDto::from)
}

/// Answer the question [`activate_session`] asked (#67).
///
/// Two answers and no others, and both are the user's: associate the instance
/// they picked, or start the Hub's own copy and leave that instance alone.
/// Cancelling is not one of them — a window that closes the dialog sends
/// nothing, and nothing is exactly what happens.
///
/// Only the *association* half needs the pid and creation time, and they are
/// re-verified rather than trusted: what came back describes a moment that has
/// passed, and `app::activation::associate` is where that is turned back into a
/// statement about a process that exists now.
#[tauri::command(async)]
pub fn resolve_session_open(
    core: State<'_, SessionCore>,
    session_id: String,
    resolution: OpenResolution,
) -> Result<ActivationDto, SessionError> {
    const OPERATION: &str = "resolve_session_open";

    let outcome = match resolution.kind {
        OpenResolutionKind::New => crate::app::activation::open_new(&core, &session_id),
        OpenResolutionKind::Associate => {
            let created_at = resolution
                .created_at
                .as_deref()
                .and_then(|created| created.parse::<u64>().ok());
            match (resolution.pid, created_at) {
                (Some(pid), Some(created_at)) => {
                    crate::app::activation::associate(&core, &session_id, pid, created_at)
                }
                _ => {
                    return Err(SessionError::failed(
                        &session_id,
                        OPERATION,
                        "associating an instance needs both its pid and its creation time, and                          this request was missing one of them"
                            .to_owned(),
                        None,
                    ))
                }
            }
        }
    }?;
    Ok(ActivationDto::from(outcome))
}

/// What opening one application answered with (#66, #67).
///
/// `runtime` is the lifecycle half the caller already knows how to read (the
/// same shape `activate_session` answered with before the window step existed);
/// `window` is present only for an entry that keeps its own window; and
/// `choice` is present only when the Hub found something outside itself it
/// will not decide about on the user's behalf (#67).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivationDto {
    pub runtime: SessionRuntime,
    /// Whether this call created the run.
    pub started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowStepDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<OpenChoiceDto>,
}

/// The question the Hub is asking instead of answering (#67).
///
/// A field rather than a separate command result, so the window reads one shape
/// for every open: "here is the session, here is what happened to its window,
/// and — when there is one — here is what the Hub could not decide".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenChoiceDto {
    /// What could not be established, in a sentence.
    pub reason: String,
    pub candidates: Vec<ExternalCandidateDto>,
}

/// One instance that might be the application already running (#67).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalCandidateDto {
    pub pid: u32,
    /// When the process started, as a decimal string.
    ///
    /// A string because this is a Windows `FILETIME` — hundreds of quadrillions
    /// of 100-ns ticks — and a JSON number is a double in the window: the value
    /// would come back through `resolve_session_open` rounded, and the identity
    /// check would then fail for the instance the user picked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the instance has a window the Hub could bring forward, so the
    /// dialog can say which candidate is one the user can actually be shown.
    pub has_window: bool,
    /// Whether associating it is something the Hub can do safely.
    ///
    /// The one field the dialog's "associate" control turns on, and the reason
    /// `verified` is not sent beside it: the flag is the *action's* answer, and
    /// what makes it false is already in `reason` in the user's words.
    pub associable: bool,
    /// Whether the arguments it was started with are the configured ones;
    /// absent when the configuration passes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_agree: Option<bool>,
    /// Why this one is uncertain, for the dialog to show beside it.
    pub reason: String,
}

/// What the user answered when the Hub asked (#67).
///
/// `kind` is a type rather than a string matched against two literals: the
/// window models the same two answers as a union, and a command that could be
/// sent a third one has a branch that can never be reached and no way to say
/// so.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResolution {
    pub kind: OpenResolutionKind,
    /// The process the user picked; required for `associate`.
    pub pid: Option<u32>,
    /// Its creation time as it was shown, as a decimal string; required for
    /// `associate`. The half that survives the pid being reused.
    pub created_at: Option<String>,
}

/// The two answers there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenResolutionKind {
    /// Take the instance the user picked.
    Associate,
    /// Start the Hub's own copy and leave the other one alone.
    New,
}

/// What bringing an application's own window forward did (#66).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowStepDto {
    /// `"focused" | "refused" | "no_window"`.
    pub outcome: WindowOutcomeDto,
    /// The window's caption, when one was found — for naming it in a notice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The process the window belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// What to tell the user when the window did not come forward.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowOutcomeDto {
    /// The window was restored if minimized and brought to the foreground.
    Focused,
    /// Windows refused the foreground change; the window is still on screen.
    Refused,
    /// The application is running without a window to bring forward.
    NoWindow,
}

impl From<crate::app::activation::OpenOutcome> for ActivationDto {
    fn from(outcome: crate::app::activation::OpenOutcome) -> Self {
        use crate::app::activation::WindowStep;

        let window = outcome.window.map(|step| {
            let (outcome, found) = match &step {
                WindowStep::Focused(window) => (WindowOutcomeDto::Focused, Some(window)),
                WindowStep::Refused(window) => (WindowOutcomeDto::Refused, Some(window)),
                WindowStep::NoWindow => (WindowOutcomeDto::NoWindow, None),
            };
            WindowStepDto {
                outcome,
                title: found.map(|window| window.title.clone()),
                pid: found.map(|window| window.pid),
                notice: step.notice(),
            }
        });
        let choice = outcome.choice.map(|choice| OpenChoiceDto {
            reason: choice.reason,
            candidates: choice
                .candidates
                .into_iter()
                .map(|candidate| {
                    let associable = candidate.associable();
                    ExternalCandidateDto {
                        pid: candidate.pid,
                        created_at: candidate.created_at.map(|created| created.to_string()),
                        file_name: candidate.file_name,
                        image_path: candidate
                            .image_path
                            .map(|path| path.to_string_lossy().into_owned()),
                        title: candidate.title,
                        has_window: candidate.has_window,
                        associable,
                        arguments_agree: candidate.arguments_agree,
                        reason: candidate.reason,
                    }
                })
                .collect(),
        });
        ActivationDto {
            runtime: outcome.activation.runtime,
            started: outcome.activation.started,
            window,
            choice,
        }
    }
}

/// Remove a temporary session from the registry (#62).
///
/// Session Core refuses a configured session (that one lives in the config
/// file) and a live one (its process tree is still owned by its handle), so
/// this command cannot be used as a way to stop something by deleting it.
#[tauri::command(async)]
pub fn remove_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<(), SessionError> {
    core.remove_session(&session_id)
}

/// One session's snapshot.
#[tauri::command]
pub fn get_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<SessionRuntime, SessionError> {
    core.snapshot(&session_id).ok_or_else(|| SessionError {
        kind: crate::session::core::SessionErrorKind::UnknownSession,
        session_id: session_id.clone(),
        operation: "get_session".to_owned(),
        message: format!("no session is registered as `{session_id}`"),
        from: None,
    })
}

/// Start a session.
#[tauri::command(async)]
pub fn start_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<SessionRuntime, SessionError> {
    core.start(&session_id)
}

/// Stop a session, giving it the default grace period to unwind.
#[tauri::command(async)]
pub fn stop_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<SessionRuntime, SessionError> {
    core.stop(&session_id)
}

/// Terminate a session's run without waiting for it to unwind.
#[tauri::command(async)]
pub fn force_stop_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<SessionRuntime, SessionError> {
    core.force_stop(&session_id)
}

/// Replace a session's run with a fresh one.
#[tauri::command(async)]
pub fn restart_session(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<SessionRuntime, SessionError> {
    core.restart(&session_id)
}

/// Open a session's configured URL with the OS's default handler (spec §9).
///
/// Takes a session id, never a URL: the session's own configuration is the only
/// place the address comes from, so this command cannot be used to send the
/// machine anywhere the user did not configure. A session without a `url` is
/// refused with what to add, rather than opening nothing (`crate::shell`'s note,
/// `docs/DECISIONS.md` D-021).
#[tauri::command(async)]
pub fn open_session_url(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<(), SessionError> {
    let url = core.session_url(&session_id)?;
    shell::open_url(&url).map_err(|error| hand_over_failed(&session_id, "open_session_url", error))
}

/// Open a session's configured working directory in the OS's file browser
/// (spec §9).
///
/// The same shape as [`open_session_url`], and for the same reason: the folder
/// is resolved from the session, so this is not a general "open a folder"
/// command. A session with no `cwd`, or one whose folder has since been deleted,
/// is reported with an actionable message.
#[tauri::command(async)]
pub fn open_session_cwd(
    core: State<'_, SessionCore>,
    session_id: String,
) -> Result<(), SessionError> {
    let path = core.session_cwd(&session_id)?;
    shell::open_path(&path)
        .map_err(|error| hand_over_failed(&session_id, "open_session_cwd", error))
}

/// Remove a saved launch entry; application files and disk logs are retained.
#[tauri::command(async)]
pub fn remove_application(
    core: State<'_, SessionCore>,
    report: State<'_, ConfigReportDto>,
    session_id: String,
) -> Result<(), SessionError> {
    let path = report.config_path.as_deref().ok_or_else(|| SessionError {
        kind: crate::session::core::SessionErrorKind::Failed,
        session_id: session_id.clone(),
        operation: "remove_application".to_owned(),
        message: "无法确定配置文件位置".to_owned(),
        from: None,
    })?;
    core.remove_application(&session_id, std::path::Path::new(path))
}
