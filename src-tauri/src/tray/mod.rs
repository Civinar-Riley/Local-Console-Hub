//! Tray layer — native tray lifecycle and quick controls
//! (T09, #10; `docs/MVP_IMPLEMENTATION_SPEC.md` §11, `docs/UI_STYLE_GUIDE.md`
//! §9).
//!
//! The Hub is meant to be a low-interruption tray-resident controller: closing
//! the main window hides it, managed sessions keep running, and the tray is the
//! way back in and the only way out (`docs/DECISIONS.md` D-006).
//!
//! ## What this layer is allowed to decide
//!
//! Nothing about lifecycle. Every control here ends in a
//! [`SessionCore`](crate::session::core::SessionCore) call — the same ones the
//! window's buttons make — and every reading here comes from Session Core's own
//! snapshots (spec §3: "Tray actions call the same Session Core APIs as the
//! main window"; "Session Core is the single source of lifecycle truth").
//!
//! What it *does* own is presentation and selection, and both are pure
//! functions with tests of their own:
//!
//! - [`model`] — what the tray shows, derived from configs and snapshots;
//! - [`menu`] — the items that reading turns into, and their ids;
//! - [`actions`] — which sessions a control is about, and what Exit costs.
//!
//! What is left in this file is the wiring those modules cannot test: creating
//! the icon, dispatching a click, and the exit sequence.
//!
//! ## Threading
//!
//! A menu click arrives on the main thread, and every control below it blocks —
//! a stop waits out its grace period, Exit waits for an answer — so the work is
//! handed to a thread of its own ([`on_worker`]). Rebuilding the menu needs no
//! such care in either direction: Tauri proxies every menu operation to the
//! main thread, and executes it inline when it is already there.

mod actions;
mod menu;
mod model;

use std::sync::Mutex;

use serde::Serialize;
use tauri::menu::MenuEvent;
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::dialog;

use crate::session::core::{EventSink, SessionCore, SessionError};
use crate::session::event::SessionEvent;

use model::TrayModel;

/// The tray icon's id. One icon, one id, for the life of the app.
pub const TRAY_ID: &str = "main-tray";

/// The label the main window is created with (`tauri.conf.json`), and the one
/// the close handler matches on.
pub const MAIN_WINDOW: &str = "main";

/// `session-focus-requested` — the tray asked the window to show one session.
///
/// A request from the tray to the window, not a session event: it says what the
/// user wants to look at, and carries no lifecycle claim at all. The window
/// selects the session it names; if nothing is listening, the window was still
/// brought back, which is the part that matters.
pub const SESSION_FOCUS_REQUESTED: &str = "session-focus-requested";

/// Payload of [`SESSION_FOCUS_REQUESTED`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFocusRequested {
    pub session_id: String,
}

/// Create the tray icon and its first menu.
///
/// Called once, from the app's `setup`, *after* the registry is managed: the
/// first menu is built from the real registry, so the tray is never briefly
/// showing an empty one.
///
/// ## Why a missing icon is fatal
///
/// The icon is the approved application identity, taken from the same icon set
/// the window, the taskbar and the installers use (D-013; the set is generated
/// from one Hub mark since #68, D-029) rather than a second asset that
/// could drift from it. It is embedded at build time from `bundle.icon`, so
/// its absence means a broken build rather than a condition to handle at
/// runtime.
///
/// Refusing to start is the honest response, and the only safe one: this same
/// `setup` installs the handler that hides the window on close, and the tray is
/// the only way back to a hidden window. An icon-less tray on Windows is an
/// entry the user cannot see, so carrying on would produce an app that
/// disappears into nowhere on the first click of its X.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let icon = app.default_window_icon().cloned().ok_or_else(|| {
        tauri::Error::AssetNotFound("the bundle's application icon (`bundle.icon`)".to_owned())
    })?;

    let model = current_model(app);
    let menu = menu::build(app, &model)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip(&model.tooltip)
        // Left click opens the menu rather than doing something invisible:
        // the tray's whole job is to say what is running and to offer a way
        // back, and both of those live in the menu.
        .show_menu_on_left_click(true)
        .on_menu_event(dispatch)
        .build(app)?;
    Ok(())
}

/// The window-event hook that turns the main window's X into a hide (D-006).
///
/// The window is *hidden*, never destroyed: the webview keeps its state, the
/// sessions keep running, and showing it again restores exactly what was there
/// — which is what "Show Window restores the same live state" means. Nothing
/// here is conditional on a run being active: hiding while everything is idle
/// is the same gesture, and asking the user to close again once they start
/// something would be a rule with no purpose.
///
/// The real exit is the tray's; §11 requires an explicit entry for it and this
/// is deliberately not one. `api.prevent_close` is what keeps the app alive
/// when the last window would otherwise go away — and with it every session the
/// app is holding.
pub fn on_window_event<R: Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    let tauri::WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if !hides_on_close(window.label()) {
        return;
    }
    api.prevent_close();
    let _ = window.hide();
}

/// Whether closing the window with `label` hides it rather than ending the app.
///
/// The rule as a predicate, so the one window it is about is stated in a place
/// a test can read: a second window added later would keep its own close
/// behaviour until someone decides otherwise here.
pub fn hides_on_close(label: &str) -> bool {
    label == MAIN_WINDOW
}

/// Session Core's events, turned into "the tray may need rebuilding".
///
/// Session Core publishes to an [`EventSink`] rather than to the tray, so this
/// is the adapter that knows about both — the same shape as
/// [`TauriSink`](crate::session::tauri_sink::TauriSink), and for the same
/// reason: the lifecycle stays runnable with no tray at all.
///
/// It keeps the last model it rendered, so an event that cannot change what the
/// tray shows costs nothing and a *pair* of events that describe one transition
/// rebuilds the menu once. That matters more than it looks: session state is
/// independent of window visibility (spec §9), so these events keep arriving
/// while the Hub sits hidden in the tray, which is exactly when §14 asks for
/// the app to be idle.
pub struct TraySink<R: Runtime> {
    app: AppHandle<R>,
    last: Mutex<Option<TrayModel>>,
}

impl<R: Runtime> TraySink<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        TraySink {
            app,
            last: Mutex::new(None),
        }
    }
}

impl<R: Runtime> EventSink for TraySink<R> {
    fn publish(&self, event: SessionEvent) {
        if !changes_the_tray(&event) {
            return;
        }
        refresh(&self.app, &self.last);
    }
}

/// Whether an event can change what the tray shows.
///
/// The tray renders sessions and their counts, so five of §9's events matter:
/// a session's state, the counts, and — since the registry's membership can
/// change while the app runs (#62) — a session entering or leaving it, plus a
/// session being saved (#65), which renames a menu row without touching the
/// counts or the membership. A run record or a batch of terminal output says
/// nothing the tray displays, and rebuilding a menu per output batch would
/// spend §14's idle budget on a surface that did not change.
pub fn changes_the_tray(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::StateChanged(_)
            | SessionEvent::Created(_)
            | SessionEvent::Removed(_)
            | SessionEvent::Saved(_)
            | SessionEvent::AppSummaryChanged(_)
    )
}

/// The tray's reading of the registry right now.
fn current_model<R: Runtime>(app: &AppHandle<R>) -> TrayModel {
    match app.try_state::<SessionCore>() {
        Some(core) => model::build(&core.configs(), &core.snapshots()),
        // Before `setup` finishes there is no registry, and that is a Hub with
        // nothing in it — not an error worth failing an icon over.
        None => model::build(&[], &[]),
    }
}

/// Rebuild the tray's menu if the registry's reading changed.
///
/// The whole read–build–set–record sequence holds `last`. Events arrive on
/// whichever thread published them — a per-session watcher, a stop worker, an
/// IPC command — so two can be in `refresh` at once, and dropping the lock
/// between the compare and the record would let the older reading overwrite
/// the newer one's menu, then make every later event look "unchanged" against
/// the record it left behind. Serialised, the last refresh in line reads a
/// registry at least as new as the model it compares against, and its build is
/// the one the tray ends up showing.
fn refresh<R: Runtime>(app: &AppHandle<R>, last: &Mutex<Option<TrayModel>>) {
    let mut last_model = lock(last);
    let model = current_model(app);
    if last_model.as_ref() == Some(&model) {
        return;
    }
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let Ok(menu) = menu::build(app, &model) else {
        return;
    };
    if tray.set_menu(Some(menu)).is_err() {
        return;
    }
    // The tooltip is a hint, not the surface: a failure there leaves the menu
    // correct, so the model is still the one on screen.
    let _ = tray.set_tooltip(Some(&model.tooltip));
    *last_model = Some(model);
}

/// Handle one tray menu click.
fn dispatch<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let id = event.id().as_ref();

    if let Some(session_id) = id.strip_prefix(menu::ids::SESSION_PREFIX) {
        let session_id = session_id.to_owned();
        on_worker(app, move |app| focus_session(app, &session_id));
        return;
    }

    match id {
        // The tray has nothing to add to the answer: a window that could not be
        // restored would leave the tray menu open in front of the user, which is
        // where the problem already is.
        menu::ids::SHOW_WINDOW => {
            show_window(app);
        }
        menu::ids::RESTART_FAILED => on_worker(app, restart_failed),
        menu::ids::STOP_ALL => on_worker(app, stop_all),
        menu::ids::EXIT => on_worker(app, exit),
        // The two readings carry ids as well, so that every item has one. They
        // are created disabled and Windows does not deliver their clicks; this
        // arm exists so a future editable reading cannot be silently added
        // without someone deciding what it does.
        _ => {}
    }
}

/// Run a control's work off the main thread.
///
/// The click arrives on the main thread and everything below it blocks: a stop
/// waits out its grace period, and Exit waits for the user to answer a modal
/// box. Blocking the event loop for that would freeze the tray that produced the
/// click, and with it the app's only way out.
fn on_worker<R: Runtime, F>(app: &AppHandle<R>, work: F)
where
    F: FnOnce(&AppHandle<R>) + Send + 'static,
{
    let app = app.clone();
    std::thread::spawn(move || work(&app));
}

/// Bring the main window back, in the state it was hidden in.
///
/// `pub(crate)` because the tray is not the only thing that can ask for the
/// window: a launch request that reached the running Hub is answered with this
/// same call ([`crate::app::launch`], #60), so "bring the Hub back" has one
/// definition rather than two that could drift apart (spec §2: 窗口、快捷方式与
/// 托盘使用同一应用操作边界).
///
/// The answer is about *issuing* the restore, and it is deliberately no more
/// than that. `show` and friends are proxied to the main thread and return
/// before the window has been drawn, so reading `is_visible` back here would
/// report the state the window was in *before* the call — a false failure on a
/// restore that is about to work. What can be said without racing is whether
/// there is a window to restore at all, and the launch path reports that much
/// rather than claiming a success it did not observe
/// ([`crate::app::launch::Hub`]).
pub(crate) fn show_window<R: Runtime>(app: &AppHandle<R>) -> bool {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return false;
    };
    // `show` alone can leave a minimized window minimized — the user asked to
    // see the workspace, not its taskbar button.
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    true
}

/// Open the window on one session.
///
/// The window is shown first and the request second, so a listener that has not
/// attached yet still gets the part that matters: the user has something to
/// look at, and the session they wanted is one click away in the rail.
///
/// Crate-visible because a launch request that opened an application ends the
/// same way (#64): the Hub is brought back *on that application*, through this
/// one definition of "show me that session" rather than a second one that
/// could drift from it (spec #59 §2).
pub(crate) fn focus_session<R: Runtime>(app: &AppHandle<R>, session_id: &str) {
    show_window(app);
    let _ = app.emit(
        SESSION_FOCUS_REQUESTED,
        SessionFocusRequested {
            session_id: session_id.to_owned(),
        },
    );
}

/// Restart every session that ended in `Error`.
///
/// A refusal is dropped on the floor on purpose: the only way it happens is a
/// race with a session that moved between the snapshot and the call, and
/// Session Core records the outcome on the session either way — the next state
/// event rebuilds the tray from it. There is nothing for the tray to add but a
/// second, staler opinion.
fn restart_failed<R: Runtime>(app: &AppHandle<R>) {
    let Some(core) = app.try_state::<SessionCore>() else {
        return;
    };
    restart_failed_in(&core);
}

/// [`restart_failed`], against the registry rather than the app.
///
/// Split out so the selection and the calls can be exercised against a real
/// `SessionCore` and real processes, with no window and no tray.
fn restart_failed_in(core: &SessionCore) {
    for session_id in actions::restart_targets(&core.snapshots(), &core.entries()) {
        let _ = core.restart(&session_id);
    }
}

/// Stop every session the state machine allows a stop from.
///
/// A failure is already recorded on the session it happened to — that is where
/// Session Core puts it, and where the next state event carries it to the tray
/// — so the tray's own Stop All has nothing to add. [`exit`] is the caller that
/// does, because it must not leave with a run still held.
fn stop_all<R: Runtime>(app: &AppHandle<R>) {
    let Some(core) = app.try_state::<SessionCore>() else {
        return;
    };
    let _ = stop_all_in(&core);
}

/// [`stop_all`], against the registry rather than the app, reporting what could
/// not be stopped.
///
/// **Concurrently**, because a stop waits out a grace period of its own:
/// stopping five services one after another would spend five grace periods on
/// a single click. Sessions are independent by design — Session Core holds one
/// lock per session and never the registry across a lifecycle operation — so
/// the stops cannot hold each other up.
///
/// A stop thread that panicked counts as a failure and never as a success:
/// "[`exit`] stops everything, then leaves" is only honest if everything it
/// believes it stopped really stopped. A panicked thread says nothing about
/// the tree it was stopping, so the caller is told about that session rather
/// than left to assume.
fn stop_all_in(core: &SessionCore) -> Vec<SessionError> {
    let stops: Vec<_> = actions::stop_targets(&core.snapshots(), &core.entries())
        .into_iter()
        .map(|session_id| {
            let core = core.clone();
            let stopping = session_id.clone();
            let handle = std::thread::spawn(move || core.stop(&stopping).err());
            (session_id, handle)
        })
        .collect();

    stops
        .into_iter()
        .filter_map(|(session_id, stop)| match stop.join() {
            Ok(failure) => failure,
            Err(_) => Some(SessionError::failed(
                &session_id,
                "stop",
                "the stop did not finish, so its process tree was never confirmed gone",
                None,
            )),
        })
        .collect()
}

/// Leave the Hub — asking first when leaving would cost a run.
///
/// §11's order, exactly: if anything is still moving, say what this is waiting
/// for and stop; if runs are active, ask; then stop everything and go. The Hub
/// only reaches `app.exit` with no managed tree left, so a run can never be
/// dropped on the floor by the process that was holding its job object.
fn exit<R: Runtime>(app: &AppHandle<R>) {
    let Some(core) = app.try_state::<SessionCore>() else {
        app.exit(0);
        return;
    };

    let plan = actions::exit_plan(&core.snapshots(), &core.entries());
    let configs = core.configs();

    if let Some(message) = plan.blocked_message(&configs) {
        dialog::report(&message);
        return;
    }
    if let Some(prompt) = plan.confirm_prompt(&configs) {
        if !dialog::confirm(&prompt) {
            return;
        }
    }

    let failures = clear_for_exit(&core);
    if !failures.is_empty() {
        dialog::report(&actions::stop_failure_message(&core.configs(), &failures));
        return;
    }

    app.exit(0);
}

/// Stop everything Exit must stop, or say what is still in the way.
///
/// Read again here rather than reusing the plan Exit asked its question from,
/// and read again *after* the stops rather than before them: answering a modal
/// question takes as long as the user takes, and starting a service is one
/// click. A session that came up `Running` while the question was on screen is
/// stopped by [`stop_all_in`]; one that is still `Starting` or `Stopping` when
/// the stops are done cannot be stopped at all, and is reported instead of
/// being left for the job object to finish off.
///
/// That leaves one gap, and it is worth naming rather than pretending it is
/// closed: a session that enters `Starting` in the moment between this check
/// and `app.exit`. Closing it needs a registry that refuses to start anything
/// while the app is leaving, which is a Session Core change this ticket did not
/// make; the window is a few microseconds wide, and the alternative — never
/// exiting while something might start — is not an app.
fn clear_for_exit(core: &SessionCore) -> Vec<SessionError> {
    let mut failures = stop_all_in(core);
    failures.extend(blockers_as_failures(
        actions::exit_plan(&core.snapshots(), &core.entries()).blockers,
    ));
    failures
}

/// The sessions Exit cannot stop, said as failures Exit can report.
///
/// A session that cannot be interrupted is not a stop that failed — the Hub
/// never asked it to stop — but at this point in the sequence it is the same
/// thing to the user: a reason the Hub did not leave.
fn blockers_as_failures(session_ids: Vec<String>) -> Vec<SessionError> {
    session_ids
        .into_iter()
        .map(|session_id| {
            SessionError::failed(
                &session_id,
                "exit",
                "still starting or stopping, so it cannot be stopped before the Hub leaves",
                None,
            )
        })
        .collect()
}

/// A poisoned lock is a panic in another thread, not a reason to lose the tray:
/// the value it guards is a display cache, so the worst a torn read can do is
/// rebuild a menu that was already correct.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EffectiveLogMode, EffectiveLogging, LogSource, SessionConfigDto};
    use crate::session::event::{
        AppSummary, AppSummaryChanged, RunRecordUpdated, SessionCreated, SessionRemoved,
        SessionSaved, TerminalOutput,
    };
    use crate::session::runtime::{RunId, RunRecord, SessionRuntime, Timestamp};
    use crate::session::terminal::OutputBatch;

    fn runtime() -> SessionRuntime {
        SessionRuntime::stopped(
            "comfyui",
            EffectiveLogging {
                mode: EffectiveLogMode::Off,
                source: LogSource::Captured,
                external_path: None,
            },
        )
    }

    fn created_config() -> SessionConfigDto {
        SessionConfigDto {
            id: "terminal-1".to_owned(),
            name: "PowerShell 1".to_owned(),
            session_type: "terminal".to_owned(),
            cwd: Some(r"C:\Users\example".to_owned()),
            command: None,
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            shell: Some("powershell".to_owned()),
            initial_command: None,
            display: "internal".to_owned(),
            lifecycle: "managed".to_owned(),
            logging: crate::config::EffectiveLoggingDto {
                mode: "off".to_owned(),
                source: "none".to_owned(),
                external_path: None,
            },
            temporary: true,
        }
    }

    /// The tray is rebuilt for the events it can render from, and for nothing
    /// else — the difference between a tray that tracks the registry and one
    /// that redraws itself on every terminal keystroke (§14).
    #[test]
    fn only_registry_and_state_events_reach_the_tray() {
        assert!(changes_the_tray(&SessionEvent::StateChanged(
            crate::session::event::SessionStateChanged {
                session_id: "comfyui".to_owned(),
                runtime: runtime(),
            }
        )));
        // #62: a temporary terminal appearing or leaving changes the tray's
        // session list, and its summary line, with no state event to follow.
        assert!(changes_the_tray(&SessionEvent::Created(SessionCreated {
            session_id: "terminal-1".to_owned(),
            config: created_config(),
        })));
        assert!(changes_the_tray(&SessionEvent::Removed(SessionRemoved {
            session_id: "terminal-1".to_owned(),
        })));
        // #65: saving a terminal renames its row in the menu, which the
        // membership events do not carry and the counts do not move.
        assert!(changes_the_tray(&SessionEvent::Saved(SessionSaved {
            session_id: "terminal-1".to_owned(),
            config: {
                let mut saved = created_config();
                saved.temporary = false;
                saved.name = "项目终端".to_owned();
                saved
            },
        })));
        assert!(changes_the_tray(&SessionEvent::AppSummaryChanged(
            AppSummaryChanged {
                summary: AppSummary {
                    total: 1,
                    running: 0,
                    error: 0,
                },
            }
        )));
        assert!(!changes_the_tray(&SessionEvent::RunRecordUpdated(
            RunRecordUpdated {
                session_id: "comfyui".to_owned(),
                run: RunRecord {
                    run_id: RunId::mint(),
                    session_id: "comfyui".to_owned(),
                    started_at: Timestamp::now(),
                    ended_at: None,
                    exit_code: None,
                    pid: Some(1),
                    log_mode: EffectiveLogMode::Off,
                    log_source: LogSource::Captured,
                    log_file: None,
                },
            }
        )));
        assert!(!changes_the_tray(&SessionEvent::TerminalOutput(
            TerminalOutput::from_batch(
                "comfyui",
                &OutputBatch {
                    generation: 1,
                    start: 0,
                    end: 1,
                    bytes: b"x".to_vec(),
                },
            )
        )));
    }

    /// The window the close handler is about is the main one; another window's
    /// close is not this layer's to intercept.
    #[test]
    fn only_the_main_window_hides_on_close() {
        assert!(hides_on_close(MAIN_WINDOW));
        assert!(!hides_on_close("other"));
    }

    /// A session Exit could not stop has to reach the user as a failure naming
    /// it — that message is the whole reason the Hub stays open instead of
    /// leaving a run inside a job object about to die.
    #[test]
    fn a_session_that_cannot_be_stopped_becomes_a_named_failure() {
        let failures = blockers_as_failures(vec!["comfyui".to_owned(), "api".to_owned()]);

        let named: Vec<&str> = failures
            .iter()
            .map(|failure| failure.session_id.as_str())
            .collect();
        assert_eq!(named, vec!["comfyui", "api"]);
        for failure in &failures {
            assert_eq!(failure.operation, "exit");
            assert!(
                failure.message.contains("starting or stopping"),
                "{}",
                failure.message
            );
        }
    }

    /// The payload the window listens for must name the session at the top
    /// level, camelCased, like every other IPC payload (`src/types/tray.ts`).
    #[test]
    fn the_focus_request_serializes_to_the_camel_case_contract() {
        let value = serde_json::to_value(SessionFocusRequested {
            session_id: "comfyui".to_owned(),
        })
        .expect("the payload serializes");

        assert_eq!(value["sessionId"], serde_json::json!("comfyui"));
        assert!(value.get("session_id").is_none(), "snake_case leaked");
    }

    // The two controls below are the tray's only bulk operations, and they are
    // the ones a test can run for real: they act on a `SessionCore` and real
    // processes, with no window and no tray. `SessionCore` is the app's own
    // type, not a stand-in, so what is asserted here is what the tray does.

    /// A child that stays alive until someone stops it — the same long-running
    /// console process the T03/T04 smoke tests use.
    const LONG_RUNNING: &str = "cmd.exe /c ping -n 120 127.0.0.1";

    fn service(id: &str, command: &str) -> crate::config::SessionConfig {
        use crate::config::{DisplayMode, LifecycleOwner, SessionType};

        crate::config::SessionConfig {
            id: id.to_owned(),
            name: format!("Service {id}"),
            session_type: SessionType::Service,
            cwd: Some(std::env::current_dir().expect("the test process has a working directory")),
            command: Some(command.to_owned()),
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            shell: None,
            initial_command: None,
            display: DisplayMode::Internal,
            lifecycle: LifecycleOwner::Managed,
            logging: EffectiveLogging {
                mode: EffectiveLogMode::Off,
                source: LogSource::Captured,
                external_path: None,
            },
        }
    }

    fn wait_for_status(
        core: &SessionCore,
        session_id: &str,
        want: crate::session::state::SessionStatus,
        timeout: std::time::Duration,
    ) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let status = core
                .snapshot(session_id)
                .expect("the session is registered")
                .status;
            if status == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session `{session_id}` never reached {} within {timeout:?}; stuck at {}",
                want.as_str(),
                status.as_str()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn is_live(core: &SessionCore, session_id: &str) -> bool {
        use crate::session::state::SessionStatus;

        let status = core.snapshot(session_id).expect("registered").status;
        matches!(
            status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping
        )
    }

    /// Stop All's contract, against real processes: every running session is
    /// stopped, and a session that was not running is not touched — T08's
    /// "actions affect only the selected managed session", extended to the one
    /// action that is deliberately about all of them.
    #[test]
    fn stop_all_stops_the_running_sessions_and_leaves_the_rest_alone() {
        use crate::session::state::SessionStatus;

        let core = SessionCore::without_listener();
        core.register(service("one", LONG_RUNNING))
            .expect("registers");
        core.register(service("two", LONG_RUNNING))
            .expect("registers");
        core.register(service("idle", "cmd.exe /c exit 0"))
            .expect("registers");
        core.start("one").expect("starts");
        core.start("two").expect("starts");

        let failures = stop_all_in(&core);

        assert!(
            failures.is_empty(),
            "nothing should have failed: {failures:?}"
        );
        assert!(!is_live(&core, "one"), "a running session was left running");
        assert!(!is_live(&core, "two"), "a running session was left running");
        assert_eq!(
            core.snapshot("idle").expect("registered").status,
            SessionStatus::Stopped,
            "a session that was never running must not be started or stopped"
        );
        assert_eq!(core.summary().running, 0);
    }

    /// Restart Failed's contract: the failed session gets a *new* run, and a
    /// healthy one keeps the run it already had — restarting that one would be
    /// the tray destroying a working service to fix a broken one.
    #[test]
    fn restart_failed_restarts_the_failed_session_and_nothing_else() {
        use crate::session::state::SessionStatus;

        let core = SessionCore::without_listener();
        core.register(service("broken", "cmd.exe /c exit 3"))
            .expect("registers");
        core.register(service("healthy", LONG_RUNNING))
            .expect("registers");
        core.start("broken").expect("starts");
        core.start("healthy").expect("starts");
        wait_for_status(
            &core,
            "broken",
            SessionStatus::Error,
            std::time::Duration::from_secs(30),
        );
        let broken_run = core.snapshot("broken").expect("registered").run_id;
        let healthy_run = core.snapshot("healthy").expect("registered").run_id;
        assert_ne!(broken_run, None, "the first run left a record");

        restart_failed_in(&core);

        // The replacement is the same command, so it fails the same way; what
        // says it was started at all is that it is a *different* run.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let restarted = core.snapshot("broken").expect("registered").run_id;
            if restarted.is_some() && restarted != broken_run {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the failed session was never started again"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            core.snapshot("healthy").expect("registered").run_id,
            healthy_run,
            "a session that did not fail must keep its own run"
        );

        // Leave nothing behind: the long-running child outlives the test
        // process if it is not stopped here.
        let _ = core.stop("healthy");
    }
}
