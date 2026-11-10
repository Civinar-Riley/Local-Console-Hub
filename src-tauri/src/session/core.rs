//! Session Core — the single source of lifecycle truth
//! (`docs/MVP_IMPLEMENTATION_SPEC.md` §3, §5).
//!
//! Everything that wants to change what a session is doing goes through this
//! type: the window, the tray, and any future scheduler. It owns the registry,
//! decides transitions against [`super::state`], drives the T03 supervisor for
//! real runs, and publishes [`SessionEvent`]s to a sink.
//!
//! ## Why the sink is a trait
//!
//! Core publishes to an [`EventSink`] rather than to a Tauri `Emitter`. That
//! keeps the lifecycle runnable with no window at all — which is both what
//! makes it testable and what satisfies "session state is independent of
//! main-window visibility": hiding the window can only mean nothing is
//! listening, never that the state machine stops running.
//!
//! ## Locking
//!
//! One lock per session, held only for the duration of one operation on that
//! session. A `stop` blocks for as long as its stop timeout, and it must not
//! be able to hold up a different session's start or the tray summary, so the
//! registry lock is never held across a lifecycle operation. Events are
//! published after the session lock is released, so a sink can never deadlock
//! against the operation that produced it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;

use crate::config::{
    self, DisplayMode, EffectiveLogMode, EffectiveLogging, LifecycleOwner, LogSource,
    SessionConfig, SessionConfigDto, SessionType,
};
use crate::health;
use crate::logging::{
    self, policy_state, BufferLimits, LogError, LogPlan, LogRoots, LogStatus, OutputSink, RunLog,
    RunLogHandle, RunOutcome, Stream, TerminalBuffer, DEFAULT_LOG_LIMITS,
};
use crate::process::independent::IndependentProcess;
use crate::process::{
    ExitStatus, ExternalProcess, ManagedProcess, OutputMode, ProcessIdentity, ProcessSpec,
    StopOutcome, StopReport,
};
use crate::pty::{Pty, PtySpec, DEFAULT_COLS, DEFAULT_ROWS, MAX_DIMENSION};
use crate::window::TopLevelWindow;

use super::event::{
    AppSummary, AppSummaryChanged, RunRecordUpdated, SessionCreated, SessionEvent, SessionRemoved,
    SessionSaved, SessionStateChanged, TerminalOutput,
};
use super::runtime::{RunId, RunRecord, SessionErrorInfo, SessionRuntime, Timestamp};
use super::state::SessionStatus;
use super::temporary::{self, ShellLookup};
use super::terminal::{
    OutputBatch, OutputRelay, RetainedChunk, TerminalAttachment, TerminalPump, PUMP_TICK,
};

/// Something that wants to hear about lifecycle changes.
///
/// Implementations must not call back into the [`SessionCore`] that produced
/// the event: events are published after the session lock is released, but
/// re-entering with an operation that waits on the same session would still be
/// a deadlock waiting to happen.
pub trait EventSink: Send + Sync + 'static {
    fn publish(&self, event: SessionEvent);
}

/// A sink that discards everything, for callers with no listener yet.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn publish(&self, _event: SessionEvent) {}
}

/// Publish every event to several sinks, in the order given.
///
/// The running app has two listeners with nothing to say to each other: the
/// window's event transport (T04) and the tray (T09). Neither is a reason for
/// Session Core to grow a second publishing path, and a collection of sinks
/// keeps "who is listening" outside the lifecycle — the same reason the sink is
/// a trait in the first place.
///
/// A sink must return: publishing is synchronous, and one that waited for
/// something would hold up the operation that produced the event as well as the
/// sinks behind it. Both current sinks return immediately (the tray's rebuild
/// is skipped outright unless the reading changed).
pub struct FanoutSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl FanoutSink {
    /// A sink forwarding to `sinks`, in order.
    pub fn new(sinks: Vec<Arc<dyn EventSink>>) -> Self {
        FanoutSink { sinks }
    }
}

impl EventSink for FanoutSink {
    fn publish(&self, event: SessionEvent) {
        for sink in &self.sinks {
            sink.publish(event.clone());
        }
    }
}

/// Why a lifecycle operation was refused or failed
/// (`docs/DEVELOPMENT.md` §9: the message names the operation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorKind {
    /// No session is registered under that id.
    UnknownSession,
    /// The session already exists; ids come from validated config and are
    /// unique there, so this means two registrations for one id.
    AlreadyRegistered,
    /// The state machine does not allow this move from the session's current
    /// state (spec §5 rule 1).
    InvalidTransition,
    /// The operation is not wired for this session type yet.
    Unsupported,
    /// The operation was allowed by the state machine but could not be carried
    /// out: an unusable command or working directory, a start the supervisor
    /// refused, a stop that could not confirm the tree was gone.
    Failed,
}

/// A refused or failed lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionError {
    pub kind: SessionErrorKind,
    pub session_id: String,
    /// The operation that was refused, e.g. `start` or `force_stop`.
    pub operation: String,
    /// Actionable, human-readable message.
    pub message: String,
    /// The state the session was in when the move was refused.
    pub from: Option<SessionStatus>,
}

impl SessionError {
    /// No session is registered under `session_id`.
    pub fn unknown_session(session_id: &str, operation: &str) -> Self {
        SessionError {
            kind: SessionErrorKind::UnknownSession,
            session_id: session_id.to_owned(),
            operation: operation.to_owned(),
            message: format!("no session is registered as `{session_id}`"),
            from: None,
        }
    }

    /// The state machine does not allow the move `from -> to`.
    pub fn invalid_transition(
        session_id: &str,
        operation: &str,
        from: SessionStatus,
        to: SessionStatus,
    ) -> Self {
        SessionError {
            kind: SessionErrorKind::InvalidTransition,
            session_id: session_id.to_owned(),
            operation: operation.to_owned(),
            message: format!(
                "cannot {operation} a session that is {} — that would be a {} -> {} \
                 transition, which the lifecycle does not allow",
                from.as_str(),
                from.as_str(),
                to.as_str()
            ),
            from: Some(from),
        }
    }

    /// The operation is not wired for this session type yet.
    pub fn unsupported(session_id: &str, operation: &str, message: impl Into<String>) -> Self {
        SessionError {
            kind: SessionErrorKind::Unsupported,
            session_id: session_id.to_owned(),
            operation: operation.to_owned(),
            message: message.into(),
            from: None,
        }
    }

    /// The operation was allowed but could not be carried out.
    pub fn failed(
        session_id: &str,
        operation: &str,
        message: impl Into<String>,
        from: Option<SessionStatus>,
    ) -> Self {
        SessionError {
            kind: SessionErrorKind::Failed,
            session_id: session_id.to_owned(),
            operation: operation.to_owned(),
            message: message.into(),
            from,
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} on session `{}` failed: {}",
            self.operation, self.session_id, self.message
        )
    }
}

impl std::error::Error for SessionError {}

/// One session's mutable state, behind its own lock.
struct SessionState {
    config: SessionConfig,
    /// Whether this session was created from the window rather than loaded
    /// from the config file (#62).
    ///
    /// Registry provenance, deliberately *not* a field of [`SessionConfig`]:
    /// that type is what the user wrote in `config.yaml`, and a temporary
    /// terminal is by definition not in there. It lives beside the session,
    /// where the two things it decides — removability, and whether the
    /// session survives a restart — belong.
    temporary: bool,
    runtime: SessionRuntime,
    /// The current run, shared with its watcher so the watcher can wait on it
    /// without holding this lock.
    run: Option<Run>,
    /// An instance of this session's application that the user was already
    /// running, which the Hub associated rather than starting a second copy
    /// (#67).
    ///
    /// Deliberately *not* a [`Run`]: a run is a handle the Hub holds on
    /// something it started, and every operation built on that (`tree_pids`,
    /// `settle_tree`, the log, the record) would have to answer a question it
    /// has no answer for. What the Hub has here is a fact about somebody
    /// else's process — that it is the application this entry names — and two
    /// things it may do with it: bring its window forward, and notice when it
    /// ends.
    adopted: Option<Adopted>,
    /// Bumped on every start. A watcher captures the generation it was started
    /// for and refuses to publish an exit that belongs to a superseded run.
    generation: u64,
    /// The current run's record, open until the run ends (spec §4). Kept here
    /// rather than in the snapshot because the snapshot is the UI's view and
    /// the record is the run-history entry.
    record: Option<RunRecord>,
    /// The bounded scrollback (`docs/LOGGING.md` §8).
    ///
    /// Per session, not per run: restarting a service must not erase what the
    /// user was reading, and §8 keeps the buffer for every session whatever
    /// the logging policy is. Shared rather than owned so a reader outside this
    /// lock can hold the scrollback while a batch arrives.
    buffer: Arc<Mutex<TerminalBuffer>>,
    /// How this session's output becomes the batches the UI is sent (T07).
    ///
    /// Driven under this lock, so the offset it reports and the scrollback it
    /// describes are always read together (`session::terminal`).
    relay: OutputRelay,
    /// The current run's log, present while a run is in flight.
    log: Option<Arc<RunLogHandle>>,
    /// The threads reading the run's pipes, if it was started with capture.
    pump_drain: Option<PumpDrain>,
    /// The thread reading a terminal's output, for a terminal run.
    terminal_pump: Option<TerminalPump>,
    /// The geometry a terminal view has asked for (T07).
    ///
    /// Remembered while no terminal is running: the view knows its own size
    /// before the session starts, and a shell spawned at the default 80×24 and
    /// corrected a moment later would render its first screen at the wrong
    /// width — for a shell that draws a progress bar or wraps a banner, the
    /// corrected screen is not the same screen. `None` means the view has not
    /// said, and the PTY layer's default applies.
    terminal_size: Option<(u16, u16)>,
    /// Why logging is not working as configured, kept after the run's log is
    /// closed: a run whose file never appeared has to stay explainable
    /// (`docs/LOGGING.md` §1.4 — the user must never be left thinking output is
    /// being recorded when it is not).
    log_problem: Option<LogError>,
}

/// An application already running outside the Hub, as this session's instance
/// (#67).
///
/// Two halves, and both are needed for different questions. The
/// [`ProcessIdentity`] is what every *number*-based lookup has to be checked
/// against first — the window search above all, because a console window is
/// found by pid (`crate::window`). The [`ExternalProcess`] is the handle the
/// ending is noticed through, and opening it already verified the identity, so
/// the wait is on the process itself rather than on a number that may since
/// have changed meaning.
#[derive(Debug, Clone)]
struct Adopted {
    identity: ProcessIdentity,
    process: Arc<ExternalProcess>,
}

impl Adopted {
    /// Whether this is still the process it was associated with.
    ///
    /// A liveness *check* rather than a liveness *test*: while the handle is
    /// open Windows cannot reuse the pid, so an ended process still matches its
    /// own identity — which is the right answer for "is this the process I
    /// associated?" (`crate::process::ProcessIdentity`).
    fn is_current(&self) -> bool {
        self.identity.matches()
    }
}

/// One session's current run, whichever kind it is.
///
/// A service runs as a supervised process (T03) and an interactive terminal as
/// a PTY (T02); the lifecycle above them is the same one, so the difference is
/// confined to this enum rather than to two copies of the state machine. The
/// methods here are exactly the questions the lifecycle asks of a run.
///
/// Cloning it clones a handle (`Arc`), never a run: a caller that needs to work
/// on a run without holding the session lock takes one of these, and the run it
/// refers to is the same run.
#[derive(Clone)]
enum Run {
    Process(Arc<ManagedProcess>),
    Terminal(Arc<Pty>),
    /// A standalone-window application the Hub starts but does not own (#66).
    /// Its lifetime is not the Hub's: dropping this handle leaves the
    /// application running (`crate::process::independent`).
    Standalone(Arc<crate::process::independent::IndependentProcess>),
}

impl Run {
    /// The terminal this run hosts, for the operations only a terminal has
    /// (typing into it, resizing it). `None` for a supervised process, which
    /// has no attached stdin in the MVP, and for a standalone application,
    /// whose console belongs to the application (`docs/DECISIONS.md` D-034).
    fn as_terminal(&self) -> Option<&Arc<Pty>> {
        match self {
            Run::Process(_) => None,
            Run::Terminal(pty) => Some(pty),
            Run::Standalone(_) => None,
        }
    }

    /// Wait up to `timeout` for the run to end on its own.
    fn wait_for_exit(&self, timeout: std::time::Duration) -> Option<ExitStatus> {
        match self {
            Run::Process(run) => run.wait_for_exit(timeout),
            Run::Terminal(pty) => pty.wait_for_exit(timeout),
            Run::Standalone(run) => run.wait_for_exit(timeout),
        }
    }

    fn exit_status(&self) -> Option<ExitStatus> {
        match self {
            Run::Process(run) => run.exit_status(),
            Run::Terminal(pty) => pty.exit_status(),
            Run::Standalone(run) => run.exit_status(),
        }
    }

    /// Whether this run still accounts for something alive — its own process,
    /// or a member of its tree.
    ///
    /// The second half matters because the two are not the same question: a
    /// terminal shell or a service can hand a child off and exit, and the run
    /// is not over while that child is alive (spec #59 decision 13). A tree
    /// that cannot be listed is not evidence of an empty one, so it counts as
    /// alive.
    fn holds_live_process(&self) -> bool {
        fn tree_not_empty<T>(listed: Result<Vec<u32>, T>) -> bool {
            match listed {
                Ok(pids) => !pids.is_empty(),
                Err(_) => true,
            }
        }

        match self {
            Run::Process(run) => run.is_running() || tree_not_empty(run.tree_pids()),
            Run::Terminal(pty) => pty.is_running() || tree_not_empty(pty.tree_pids()),
            // A standalone run reports its own end only once its tree is empty
            // (`crate::process::independent`), so its status already covers
            // the tree.
            Run::Standalone(run) => run.is_running(),
        }
    }

    /// End the run, waiting at most `timeout` for a graceful exit.
    ///
    /// The two kinds do not share a ladder, and that is the platform's answer
    /// rather than a shortcut here: a supervised service is asked to stop and
    /// then has to be terminated (`docs/DECISIONS.md` D-007), while a terminal's
    /// graceful gesture is *input* — Ctrl+C, which the user sends through the
    /// keyboard and which does not close the session (spec §7). Closing a
    /// terminal is closing the console, so there is nothing to ask gracefully;
    /// [`Pty::kill`] terminates the terminal's process tree and confirms it is
    /// gone.
    fn stop(&self, timeout: std::time::Duration) -> Result<StopReport, String> {
        match self {
            Run::Process(run) => run.stop(timeout).map_err(|error| error.to_string()),
            Run::Terminal(pty) => stop_terminal(pty),
            // A standalone application's graceful step is its own gesture —
            // closing its window, or a console interrupt when it has none —
            // and its force path is the same exact tree termination every other
            // run gets (`crate::process::independent`).
            Run::Standalone(run) => run.stop(timeout).map_err(|error| error.to_string()),
        }
    }

    /// End the run now, without waiting for it to unwind.
    fn force_stop(&self) -> Result<StopReport, String> {
        match self {
            Run::Process(run) => run.force_stop().map_err(|error| error.to_string()),
            Run::Terminal(pty) => stop_terminal(pty),
            Run::Standalone(run) => run.force_stop().map_err(|error| error.to_string()),
        }
    }

    /// End whatever of this run is still alive after its own process has gone.
    ///
    /// The process the Hub watched is not necessarily the whole run: a terminal
    /// shell can hand a child off and exit, and the session has not ended while
    /// that child runs (spec #59 decision 13). This is the barrier between the
    /// two, called once the run's own exit has been observed so nothing of the
    /// run outlives the ending that reports it.
    ///
    /// A supervised service is deliberately left alone. Its exit rules are
    /// D-007's, a service that ends on its own is reported through its own
    /// watcher, and widening this to services would change how a service that
    /// leaves a detached child behind is reported — which is not what the
    /// terminal's close semantics were asked to settle.
    fn settle_tree(&self) -> Result<(), String> {
        match self {
            Run::Process(_) => Ok(()),
            Run::Terminal(pty) => pty.end_tree().map_err(|error| error.to_string()),
            // Nothing to settle: a standalone run reports its own end only once
            // its tree is empty (`crate::process::independent`), so the barrier
            // has already been reached by the time an ending is published.
            Run::Standalone(_) => Ok(()),
        }
    }

    /// The window this run's application presents, if it has one (#66).
    ///
    /// `None` for every run that is not a standalone application: a supervised
    /// service and an interactive terminal are displayed in the Hub window, and
    /// there is no second window of theirs to bring forward.
    fn application_window(&self, timeout: std::time::Duration) -> Option<TopLevelWindow> {
        match self {
            Run::Process(_) | Run::Terminal(_) => None,
            Run::Standalone(run) => run.wait_for_window(timeout),
        }
    }
}

/// Close a terminal and confirm its process tree is gone.
///
/// `Pty::kill` is its own barrier — it returns `Ok` only once the shell's exit
/// *and* the absence of everything the shell started have been observed
/// (`crate::pty`) — so the report describes an outcome that has already
/// happened. `graceful_delivered` is `false` because no signal was delivered:
/// there is no graceful channel to a console, and claiming one would tell the
/// UI a courtesy was extended that never was (the same reason the process
/// layer reports that flag rather than assuming it).
fn stop_terminal(pty: &Pty) -> Result<StopReport, String> {
    let already_exited = pty.exit_status();
    pty.kill().map_err(|error| error.to_string())?;
    Ok(StopReport {
        outcome: match already_exited {
            Some(_) => StopOutcome::AlreadyExited,
            None => StopOutcome::Exited,
        },
        exit: pty.exit_status().unwrap_or(ExitStatus { code: None }),
        graceful_delivered: false,
    })
}

/// What a start will spawn, decided before anything is mutated.
///
/// Resolving this first is what keeps "a session that cannot start" from
/// passing through `Starting`: an unusable shell or command is refused while
/// the session is still exactly as it was.
#[derive(Debug)]
enum StartSpec {
    Process(ProcessSpec),
    Terminal(PtySpec),
    /// A standalone-window application: the same command, hosted by a run the
    /// Hub does not own (#66).
    Standalone(ProcessSpec),
}

/// A run that has been spawned but not yet wired into its session.
enum Spawned {
    Process(ManagedProcess),
    Terminal(Arc<Pty>),
    Standalone(crate::process::independent::IndependentProcess),
}

impl Spawned {
    fn pid(&self) -> u32 {
        match self {
            Spawned::Process(run) => run.pid(),
            Spawned::Terminal(pty) => pty.pid(),
            Spawned::Standalone(run) => run.pid(),
        }
    }
}

/// What a successful start still has to wire, once the state is `Running`.
///
/// The generation travels with it so the watcher that is started afterwards
/// waits on the run this start produced, not on whatever replaced it.
enum Started {
    Process {
        output: Option<crate::process::ProcessOutput>,
        generation: u64,
    },
    Terminal {
        pty: Arc<Pty>,
        generation: u64,
        /// The configured `initial_command`, typed once the terminal is up.
        initial: Option<String>,
    },
    /// A standalone application: nothing is wired, because nothing about it
    /// passes through the Hub — no pipes to pump, no terminal to type into
    /// (spec #59 decision 16).
    Standalone { generation: u64 },
}

impl Started {
    fn generation(&self) -> u64 {
        match self {
            Started::Process { generation, .. }
            | Started::Terminal { generation, .. }
            | Started::Standalone { generation } => *generation,
        }
    }
}

/// Build the spec for a startable session, or explain why it has none.
///
/// The session's type decides which layer hosts it: a service is a supervised
/// process (T03), an interactive terminal is a PTY (T02, T07). One dispatch
/// rather than a type check inside each builder, so neither builder can be
/// asked to host something it does not own.
///
/// A service that is configured to keep its own window (#66) is the same
/// command hosted differently: the process layer still owns its tree, but under
/// a job that does not end it with the Hub, and with a console of its own
/// instead of the Hub's pipes.
fn start_spec(
    config: &SessionConfig,
    terminal_size: Option<(u16, u16)>,
    id: &str,
    planned: &PlannedRun,
) -> Result<StartSpec, SessionError> {
    match config.session_type {
        SessionType::Service => {
            let spec = process_spec(config, id, planned)?;
            match config.display {
                DisplayMode::Internal => Ok(StartSpec::Process(spec)),
                DisplayMode::Window => Ok(StartSpec::Standalone(spec)),
            }
        }
        SessionType::Terminal => terminal_spec(config, terminal_size, id).map(StartSpec::Terminal),
    }
}

/// Build the spec for an interactive terminal.
///
/// `shell` may carry arguments (`pwsh -NoProfile`), split exactly as a
/// service's `command` is, so the two configuration forms behave alike. The
/// geometry is the one a view asked for, or the PTY layer's default before any
/// view has said (T07).
fn terminal_spec(
    config: &SessionConfig,
    terminal_size: Option<(u16, u16)>,
    id: &str,
) -> Result<PtySpec, SessionError> {
    const OPERATION: &str = "start";

    // Validation (T01) guarantees both of these for a terminal session; the
    // checks are here for the same reason `process_spec` has them — the
    // message a user needs if a config ever reaches here through some other
    // path, rather than a panic or a shell in the wrong directory.
    let shell = config
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .ok_or_else(|| {
            SessionError::failed(
                id,
                OPERATION,
                format!("session `{id}` has no `shell` to host on a terminal"),
                None,
            )
        })?;
    let Some(cwd) = config.cwd.clone() else {
        return Err(SessionError::failed(
            id,
            OPERATION,
            format!(
                "session `{id}` has no `cwd`; add one so the terminal opens in a known \
                 directory"
            ),
            None,
        ));
    };
    let (program, args) = split_command(shell).map_err(|reason| {
        SessionError::failed(
            id,
            OPERATION,
            format!("session `{id}` has an unusable shell: {reason}"),
            None,
        )
    })?;

    let (cols, rows) = terminal_size.unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    Ok(PtySpec::new(program, cwd)
        .with_args(args)
        .with_size(cols, rows))
}

/// Read an interactive terminal's output into its session, until it stops.
///
/// The loop is the terminal's whole output path: it takes what the console
/// host produced, hands it to the session (which is where the scrollback, the
/// run log and the batches the UI is sent all come from), and publishes what
/// is due on every tick. It ends when the run is over — the caller sets the
/// flag once the run has been finalized — or when the console host itself has
/// gone away.
fn pump_terminal(
    core: SessionCore,
    session_id: String,
    generation: u64,
    pty: Arc<Pty>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    loop {
        match pty.read_output(PUMP_TICK) {
            Some(chunk) => core.take_output_batch(&session_id, generation, Stream::Stdout, &chunk),
            // Nothing arrived: this is the tick that publishes output which has
            // waited out its batch window.
            None => core.flush_output(&session_id, generation, false),
        }
        if stop.load(Ordering::Acquire) || pty.output_ended() {
            break;
        }
    }
    // The last bytes of a run must not wait for a window that will never be
    // needed again.
    core.flush_output(&session_id, generation, true);
}

/// The registry plus everything needed to publish.
///
/// Cheap to clone: every clone shares the same sessions, which is how a
/// watcher thread and the IPC layer address the same state.
#[derive(Clone)]
pub struct SessionCore {
    sessions: Arc<Mutex<BTreeMap<String, Arc<Mutex<SessionState>>>>>,
    sink: Arc<dyn EventSink>,
    /// Where Hub-written logs and run metadata go, or `None` for a core with
    /// nowhere to write — a test, or a headless build with no app-data
    /// directory. A rootless core resolves every policy that needs a file to
    /// `off` and says so rather than writing into the current directory.
    roots: Option<LogRoots>,
    /// How much scrollback a session keeps (`docs/LOGGING.md` §8).
    scrollback: BufferLimits,
    /// How large one run's log file may grow before it is closed off.
    limits: logging::LogLimits,
    /// How often a running service's health is re-read (`crate::health`).
    health_interval: std::time::Duration,
}

/// One session's configuration together with where it came from.
///
/// The window renders a row from a configuration plus a snapshot; this is the
/// configuration half as [`SessionCore::entries`] answers it, and the two are
/// read from one lock hold so a row cannot be assembled from two registries.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub config: SessionConfig,
    /// Created from the window, not loaded from the config file (#62).
    pub temporary: bool,
}

impl SessionEntry {
    /// The configuration as the frontend reads it, flag and all.
    ///
    /// The one place a [`SessionConfig`] becomes a listing row, so a surface
    /// that shows "which of these are temporary" cannot be written against a
    /// conversion that forgot the flag.
    pub fn config_dto(&self) -> SessionConfigDto {
        let dto = SessionConfigDto::from(&self.config);
        if self.temporary {
            dto.temporary()
        } else {
            dto
        }
    }
}

/// What a session created on demand answered with.
///
/// Both halves of the new session: the window selects it by id and renders it
/// from the same pair, so nothing has to follow the answer with a read of a
/// list that may not have caught up yet (spec #59 decision 4 — "创建返回稳定
/// 会话身份与真实状态").
#[derive(Debug, Clone)]
pub struct CreatedSession {
    pub config: SessionConfig,
    /// The snapshotted state after the session was started — real, not
    /// optimistic: a creation that could not start comes back as the error
    /// that stopped it instead.
    pub runtime: SessionRuntime,
}

/// What asking an application to be open answered with (#64, spec #59
/// decision 10).
///
/// Opening is idempotent: a session that is already running — or already
/// starting — answers with the run it has instead of making a second one, and
/// only a session with nothing in flight is started. `started` says which of
/// the two happened, so a caller can tell "I opened it" from "it was already
/// open" without comparing timestamps or run ids.
#[derive(Debug, Clone)]
pub struct Activation {
    /// The session's state after the call: the new run's for a start, the
    /// existing one's for an activation that found it already open.
    pub runtime: SessionRuntime,
    /// Whether this call created the run.
    pub started: bool,
}

/// Whether a session can be taken out of the registry (#62).
///
/// `Stopped` and `Exited` are only ever reached through an ending that was
/// observed and confirmed — the terminal's own tree included, which is what
/// `Pty::kill` returning `Ok` means (D-028) — so those are the states a
/// session may be forgotten from.
///
/// `Error` is not one of them, because it says two different things: a start
/// that never got a run (which is nothing to own, so it may be removed) and a
/// **stop that could not confirm the tree was gone** (`RunEnding::Failed`,
/// where the handle is still the only thing accounting for a live process
/// tree). Dropping that handle to tidy a list is exactly the "close without
/// settling ownership" #61 exists to prevent — and #62 forbids it explicitly
/// ("使用 #61 的安全结束能力，不绕过归属"). The run is what tells the two
/// apart: a failed start leaves none, a failed stop still holds one.
///
/// A configured session is never removable, whatever its state; that is
/// [`SessionCore::remove_session`]'s caller-facing answer rather than this
/// predicate's, and both are checked.
fn removable(temporary: bool, status: SessionStatus, owns_run: bool) -> bool {
    temporary
        && match status {
            SessionStatus::Stopped | SessionStatus::Exited => true,
            SessionStatus::Error => !owns_run,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping => false,
        }
}

/// Refuse a lifecycle action on a run the Hub does not own (#66).
///
/// The lifecycle owner is a property of the entry (`docs/DECISIONS.md` D-034),
/// and an `independent` one means exactly this: the Hub started the
/// application, and the application ends itself. Saying so is the answer rather
/// than a hidden button, because the two ways out — closing the application's
/// own window, or turning management on in the configuration — are both things
/// the user can do and neither is guessable from a refusal alone.
///
/// Enforced here rather than in the window because Session Core is the single
/// source of lifecycle truth (`docs/MVP_IMPLEMENTATION_SPEC.md` §3): a rule the
/// UI applied would be one the tray, the launch path and any future caller
/// could each decide differently.
fn require_managed(
    handle: &Arc<Mutex<SessionState>>,
    session_id: &str,
    operation: &str,
) -> Result<(), SessionError> {
    let state = lock(handle);
    if state.config.is_managed() {
        return Ok(());
    }
    Err(SessionError::unsupported(
        session_id,
        operation,
        format!(
            "session `{session_id}` is a standalone-window application the Hub does not manage; \
             it ends itself. Close its own window, or add `lifecycle: managed` to its entry so \
             the Hub may stop and restart it"
        ),
    ))
}

/// Refuse a lifecycle action on an instance the Hub did not start (#67).
///
/// Associating an application the user was already running is not the same as
/// taking ownership of it. Decision 11 says so from both sides: 关联已有实例不自动
/// 取得终止权限, and 关联不自动扩大停止／退出范围. What the Hub gained is the
/// ability to *report* the instance and bring its window forward; what it did
/// not gain is a handle on the process tree it would need to end it — and this
/// layer is where "I cannot end what I did not start" is enforced rather than
/// left to each caller to remember.
///
/// The two ways out are both things the user can do and neither is guessable
/// from a refusal alone, so the message names them: close the application
/// itself, or let the Hub start its own copy next time.
fn require_owned(
    handle: &Arc<Mutex<SessionState>>,
    session_id: &str,
    operation: &str,
) -> Result<(), SessionError> {
    let state = lock(handle);
    if state.adopted.is_none() {
        return Ok(());
    }
    Err(SessionError::unsupported(
        session_id,
        operation,
        format!(
            "session `{session_id}` is associated with an instance that was already running \
             outside the Hub; the Hub did not start it and will not end it. Close the \
             application itself, or start the Hub's own copy"
        ),
    ))
}

/// Lock a mutex, surviving a previous holder's panic.
///
/// A poisoned lock means some other thread panicked mid-operation. Refusing to
/// serve any further request would take the whole app down with it, which is a
/// worse outcome than reading state that is merely stale.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a running session is holding, copied out so the process tree can be
/// listed without the session lock.
enum ListeningHold {
    Run(Run),
    /// An instance the Hub did not start. There is no job to ask (#67).
    Adopted(ProcessIdentity),
}

/// One running session, as [`crate::listen::attribute`] wants it.
///
/// The run's own identity is the one the run already stores when it has one.
/// A supervised process and a terminal only store a pid, and the identity is
/// that pid plus the creation time read now — safe while the run still holds
/// the process, which is what `Running` means here. `None` from
/// [`tree_pids`](crate::process::ManagedProcess::tree_pids) stays `None`.
fn listening_process(id: String, hold: ListeningHold) -> Option<crate::listen::SessionProcess> {
    match hold {
        ListeningHold::Adopted(identity) => {
            // No job. The parent-pid walk is the tree attribution already
            // accepts for a process the Hub did not start. `descendants`
            // cannot tell a failed table from an empty one, so a walk that
            // comes back empty is recorded as an empty list — the same answer
            // the window lookup uses — and the session's own process is still
            // recognized from `identity`. A member whose creation time cannot
            // be read makes the whole tree `None`, not a shorter list.
            let tree = recorded_members(crate::process::descendants(identity.pid()));
            Some(crate::listen::SessionProcess { id, identity, tree })
        }
        ListeningHold::Run(Run::Standalone(process)) => {
            let identity = process.identity();
            let tree = match process.tree_pids() {
                Ok(pids) => recorded_members(pids),
                Err(_) => None,
            };
            Some(crate::listen::SessionProcess { id, identity, tree })
        }
        ListeningHold::Run(Run::Process(process)) => {
            from_pid(id, process.pid(), process.tree_pids().ok())
        }
        ListeningHold::Run(Run::Terminal(pty)) => from_pid(id, pty.pid(), pty.tree_pids().ok()),
    }
}

/// `listed` is `None` when the tree could not be read. A listed tree whose
/// members cannot all be recorded is `None` as well, not a shorter `Some`.
fn from_pid(
    id: String,
    pid: u32,
    listed: Option<Vec<u32>>,
) -> Option<crate::listen::SessionProcess> {
    let created = crate::process::creation_time_of(pid)?;
    Some(crate::listen::SessionProcess {
        id,
        identity: ProcessIdentity::recorded(pid, created),
        tree: listed.and_then(recorded_members),
    })
}

/// `None` when any listed pid cannot be recorded.
///
/// Dropping that pid and returning the rest would let attribution call a
/// listener on the dropped pid external. An empty input is a listed tree with
/// no members, which is `Some([])`, not an unread tree.
fn recorded_members(pids: Vec<u32>) -> Option<Vec<ProcessIdentity>> {
    record_known(
        pids.into_iter()
            .map(|pid| crate::process::creation_time_of(pid).map(|created| (pid, created))),
    )
}

/// The same rule as [`recorded_members`], with the OS read already done.
fn record_known(
    members: impl IntoIterator<Item = Option<(u32, u64)>>,
) -> Option<Vec<ProcessIdentity>> {
    let mut recorded = Vec::new();
    for member in members {
        let (pid, created) = member?;
        recorded.push(ProcessIdentity::recorded(pid, created));
    }
    Some(recorded)
}

impl SessionCore {
    /// A core that publishes to `sink`, with nowhere to write logs.
    ///
    /// A core with no roots cannot persist anything, and a session configured
    /// to persist resolves to `off` with a reported reason rather than writing
    /// a log somewhere unintended. Production builds add the app-data roots
    /// with [`SessionCore::with_log_roots`]; the buffered scrollback every
    /// session has is unaffected either way (D-004).
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        SessionCore {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            sink,
            roots: None,
            scrollback: crate::logging::DEFAULT_BUFFER_LIMITS,
            limits: DEFAULT_LOG_LIMITS,
            health_interval: health::POLL_INTERVAL,
        }
    }

    /// A core with no listener.
    pub fn without_listener() -> Self {
        SessionCore::new(Arc::new(NoopSink))
    }

    /// Give this core the app-data roots its runs write logs and metadata to.
    ///
    /// Builder-style, and applied before anything is registered: the roots are
    /// read when a run starts, so changing them later would give two runs of
    /// one session different layouts.
    pub fn with_log_roots(mut self, roots: LogRoots) -> Self {
        self.roots = Some(roots);
        self
    }

    /// Builder-style: replace the buffer and file-size limits.
    ///
    /// Only tests do this. `docs/LOGGING.md` §9 leaves the numbers to
    /// implementation, and a value a user could change would be a value they
    /// could accidentally set to "keep everything".
    #[cfg(test)]
    fn with_limits(mut self, scrollback: BufferLimits, limits: logging::LogLimits) -> Self {
        self.scrollback = scrollback;
        self.limits = limits;
        self
    }

    /// Builder-style: replace the health poll interval.
    ///
    /// Only tests do this. The interval is a deliberate product trade rather
    /// than a setting (`crate::health::POLL_INTERVAL`); a test that had to wait
    /// five seconds per observation would be testing the constant, not the
    /// behaviour.
    #[cfg(test)]
    fn with_health_interval(mut self, interval: std::time::Duration) -> Self {
        self.health_interval = interval;
        self
    }

    /// Add a validated session to the registry.
    ///
    /// Registration is not a lifecycle operation: the session starts
    /// `Stopped`, and no process exists until [`SessionCore::start`]. Duplicate
    /// ids are refused rather than replaced — replacing a running session would
    /// drop the only handle accounting for its process.
    pub fn register(&self, mut config: SessionConfig) -> Result<SessionRuntime, SessionError> {
        self.register_with_provenance(&mut config, false)
    }

    /// Register a session created from the window rather than read from the
    /// config file (#62).
    ///
    /// The same registration in every other respect — same validation
    /// requirements, same lifecycle, same stop rules. Only its provenance
    /// differs, and only three things read that: the frontend's "remove this
    /// one" control, the create answer, and the fact that nothing ever writes
    /// it to a file.
    fn register_temporary(
        &self,
        config: &mut SessionConfig,
    ) -> Result<SessionRuntime, SessionError> {
        self.register_with_provenance(config, true)
    }

    fn register_with_provenance(
        &self,
        config: &mut SessionConfig,
        temporary: bool,
    ) -> Result<SessionRuntime, SessionError> {
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&config.id) {
            return Err(SessionError {
                kind: SessionErrorKind::AlreadyRegistered,
                session_id: config.id.clone(),
                operation: "register".to_owned(),
                message: format!("session `{}` is already registered", config.id),
                from: None,
            });
        }

        if temporary {
            // Names follow the rows that still exist, while ids keep their
            // unique sequence. Choose and reserve the first vacant ordinal
            // under the registry lock so concurrent creations cannot collide.
            let names: std::collections::BTreeSet<String> = sessions
                .values()
                .map(|state| lock(state).config.name.clone())
                .collect();
            config.name = (1u64..)
                .map(|ordinal| format!("PowerShell {ordinal}"))
                .find(|name| !names.contains(name))
                .expect("a finite registry has an unused terminal name");
        }

        let runtime = SessionRuntime::stopped(config.id.clone(), config.logging.clone());
        let id = config.id.clone();
        sessions.insert(
            id.clone(),
            Arc::new(Mutex::new(SessionState {
                config: config.clone(),
                temporary,
                runtime: runtime.clone(),
                run: None,
                adopted: None,
                generation: 0,
                record: None,
                buffer: Arc::new(Mutex::new(TerminalBuffer::new(self.scrollback))),
                relay: OutputRelay::new(),
                log: None,
                pump_drain: None,
                terminal_pump: None,
                terminal_size: None,
                log_problem: None,
            })),
        );
        // Registration creates no directory and no file: a session that has
        // never run has nothing on disk, which is how "opening a shell does not
        // leave litter behind" stays true (D-004, `docs/LOGGING.md` §1.2). The
        // folder a session's logs *will* live in is computed, not created, and
        // reported through its log status (D-011).
        Ok(runtime)
    }

    /// One session's current snapshot.
    pub fn snapshot(&self, session_id: &str) -> Option<SessionRuntime> {
        self.handle(session_id)
            .map(|state| lock(&state).runtime.clone())
    }

    /// Every session's snapshot, in id order.
    pub fn snapshots(&self) -> Vec<SessionRuntime> {
        lock(&self.sessions)
            .values()
            .map(|state| lock(state).runtime.clone())
            .collect()
    }

    /// Sessions that still account for a live process, as listener attribution
    /// sees them (#98).
    ///
    /// A reading only. It does not start, stop, signal, or replace a run.
    /// Each entry is the session id, the process identity already held for
    /// that run, and the tree listed at this moment. A tree that cannot be
    /// listed is `None`, which attribution treats as unchecked rather than
    /// empty. Members are recorded with the creation time observed now, not
    /// as bare pids.
    ///
    /// The gate is the run's own liveness, not the lifecycle status. A run the
    /// stop ladder has not confirmed gone stays owned by its session in
    /// `Error`/`Stopping`, and a terminal whose shell exited can still have a
    /// live child; both are processes the port page must attribute to the
    /// session — and, for `confirm_end_occupant`, must protect from being ended
    /// from outside it (#129). A run whose handles outlive an ended process is
    /// excluded by the same check, so a reused pid cannot be attributed to a
    /// session that no longer owns anything.
    pub fn listening_processes(&self) -> Vec<crate::listen::SessionProcess> {
        let handles: Vec<(String, Arc<Mutex<SessionState>>)> = lock(&self.sessions)
            .iter()
            .map(|(id, handle)| (id.clone(), Arc::clone(handle)))
            .collect();

        let mut listed = Vec::new();
        for (id, handle) in handles {
            let hold = {
                let state = lock(&handle);
                if let Some(adopted) = &state.adopted {
                    ListeningHold::Adopted(adopted.identity)
                } else if let Some(run) = state.run.clone().filter(|run| run.holds_live_process()) {
                    ListeningHold::Run(run)
                } else {
                    continue;
                }
            };
            if let Some(process) = listening_process(id, hold) {
                listed.push(process);
            }
        }
        listed
    }

    /// App-wide counts. Derived from the same state the snapshots come from,
    /// so the tray can never disagree with the window — including the counting
    /// rule itself, which lives in [`AppSummary::of`] rather than here.
    pub fn summary(&self) -> AppSummary {
        AppSummary::of(&self.snapshots())
    }

    fn handle(&self, session_id: &str) -> Option<Arc<Mutex<SessionState>>> {
        lock(&self.sessions).get(session_id).cloned()
    }

    /// Publish the session's post-operation snapshot and the summary it
    /// implies.
    ///
    /// Both go out together so a listener tracking only the summary cannot
    /// observe the state change and the count in the opposite order. Callers
    /// must have released the session lock: this reads every session, and the
    /// lock order everywhere else is registry-then-session.
    fn publish_state_and_summary(&self, session_id: &str) {
        if !self.publish_state(session_id) {
            return;
        }
        self.publish_summary();
    }

    /// Publish the app-wide counts.
    ///
    /// The one place the summary is sent, so a listener tracking the counts
    /// cannot be told about a change no event announced.
    fn publish_summary(&self) {
        self.sink
            .publish(SessionEvent::AppSummaryChanged(AppSummaryChanged {
                summary: self.summary(),
            }));
    }

    /// Announce that a session entered the registry (#62).
    ///
    /// The configuration travels with it because that is the half of a session
    /// a state event cannot carry: a listener that has never seen this id has
    /// no name to render, and reading `list_session_configs` to find one would
    /// make every listener poll for a change it was just told about.
    pub(crate) fn publish_created(&self, session_id: &str) {
        let Some(entry) = self.session_entry(session_id) else {
            return;
        };
        self.sink.publish(SessionEvent::Created(SessionCreated {
            session_id: session_id.to_owned(),
            config: entry.config_dto(),
        }));
        self.publish_summary();
    }

    /// Announce that a session's configuration is now a saved one (#65).
    ///
    /// The membership counts do not move, so unlike its two neighbours this
    /// publishes no summary: nothing about "how many sessions are there, how
    /// many are running" changed. What changed is what one row *is*, and that
    /// is the whole payload.
    ///
    /// Sent after the config file holds the entry, never before — the event is
    /// the window's licence to render a row as saved, and a window told that
    /// before the file agreed would be showing the user a save that a restart
    /// would not keep.
    pub(crate) fn publish_saved(&self, session_id: &str) {
        let Some(entry) = self.session_entry(session_id) else {
            return;
        };
        self.sink.publish(SessionEvent::Saved(SessionSaved {
            session_id: session_id.to_owned(),
            config: entry.config_dto(),
        }));
    }

    /// Announce that a session left the registry (#62).
    ///
    /// Sent after the registry no longer holds it, which is what makes a
    /// removal final: anything that publishes about that id afterwards —
    /// a log closing, a terminal reading its last bytes, a watcher's late
    /// exit — finds no session and publishes nothing.
    fn publish_removed(&self, session_id: &str) {
        self.sink.publish(SessionEvent::Removed(SessionRemoved {
            session_id: session_id.to_owned(),
        }));
        self.publish_summary();
    }

    /// Publish one session's current snapshot. `false` if there is no such
    /// session.
    ///
    /// Split out of [`SessionCore::publish_state_and_summary`] for the events
    /// that move nothing the summary counts: a health reading changes within a
    /// run, so the running/error totals it would republish are the same ones
    /// (T08 §12).
    fn publish_state(&self, session_id: &str) -> bool {
        let Some(runtime) = self.snapshot(session_id) else {
            return false;
        };
        self.sink
            .publish(SessionEvent::StateChanged(SessionStateChanged {
                session_id: runtime.session_id.clone(),
                runtime,
            }));
        true
    }

    /// Read a running service's health and publish it if the reading moved.
    ///
    /// Only the *changes* go out: a steady service costs one loopback connect
    /// per interval and no UI work at all, which is what §14 asks for. When
    /// the session names an http(s) URL, the same pass also GETs that URL and
    /// stores the result beside the TCP reading; it does not replace it. The
    /// reading is stored on the snapshot before anything is published, so a
    /// listener that reacts to the event and re-reads the snapshot sees the
    /// reading the event was about.
    fn read_health(
        &self,
        session_id: &str,
        handle: &Arc<Mutex<SessionState>>,
        run: &Run,
        port: u16,
        generation: u64,
    ) {
        // Copy the URL and drop the lock before either probe. Both can wait
        // out `health::PROBE_TIMEOUT`, and this thread must not hold the
        // session lock across that.
        let url = lock(handle).config.url.clone();
        let reading =
            health::ServiceHealth::read(run.exit_status().is_none(), port, url.as_deref());
        // The probe takes time and holds no lock, so the session may have moved
        // on while it was in flight; recording it under the session's own lock
        // is what decides whether this reading is still about anything.
        if lock(handle).record_health(generation, reading) {
            self.publish_state(session_id);
        }
    }

    /// Publish the current run record, if the session has one.
    fn publish_run(&self, session_id: &str) {
        let Some(handle) = self.handle(session_id) else {
            return;
        };
        let record = lock(&handle).record.clone();
        if let Some(run) = record {
            self.sink
                .publish(SessionEvent::RunRecordUpdated(RunRecordUpdated {
                    session_id: run.session_id.clone(),
                    run,
                }));
        }
    }

    /// Announce that a run has ended.
    ///
    /// Every path that ends a run goes through here — a start that failed, a
    /// stop the user asked for, and the watcher seeing a run end on its own.
    /// Splitting these apart is what let a stop close its run record in silence
    /// once already: the watcher declines to report an ending a stop owns, so a
    /// stop that forgot to publish left the closing of the record unreported
    /// everywhere.
    fn publish_ending(&self, session_id: &str) {
        self.publish_state_and_summary(session_id);
        self.publish_run(session_id);
    }

    /// Start a service session, and publish the states it passes through.
    ///
    /// On success the session is `Running` with a fresh run id and the run's
    /// process id. A refusal (unknown session, a transition the state machine
    /// does not allow, an unusable command) leaves the session exactly as it
    /// was; a spawn that fails moves it to `Error` with the reason recorded,
    /// because by then the session really is no longer stopped.
    pub fn start(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        let sessions = lock(&self.sessions);
        let handle = sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::unknown_session(session_id, "start"))?;

        // Claim the transition under the lock, then let go of it: a spawn is
        // the slow part, and holding the session lock across it would stall
        // nothing but this session's own watcher — but there is no reason to.
        //
        // Once `Starting` is published no other operation can move this
        // session: `Starting -> Starting` and `Starting -> Stopping` are both
        // outside the table, so a concurrent start or stop is refused rather
        // than interleaved with the spawn.
        let (spec, planned) = {
            let mut state = lock(&handle);

            let from = state.runtime.status;
            if !from.can_transition_to(SessionStatus::Starting) {
                return Err(SessionError::invalid_transition(
                    session_id,
                    "start",
                    from,
                    SessionStatus::Starting,
                ));
            }

            // `Error -> Starting` is for a session with nothing left alive. A
            // failed stop can leave an `Error` that still owns its run, and
            // starting over would replace that handle — terminating the tree it
            // accounts for while this lock is held. The recovery is the stop
            // ladder the `Error -> Stopping` row exists for, or a restart,
            // which performs one. A start must not quietly do either.
            if from == SessionStatus::Error
                && state.run.as_ref().is_some_and(Run::holds_live_process)
            {
                return Err(SessionError::failed(
                    session_id,
                    "start",
                    format!(
                        "session `{session_id}` still owns a live run its last stop could not \
                         confirm ended; stop or force stop it before starting it again"
                    ),
                    Some(SessionStatus::Error),
                ));
            }

            // The run's identity and its logging are settled before anything
            // is started: the run id and start time are what name its log file
            // (D-011), and a session whose policy cannot be honoured should
            // find that out before it has a process, not after.
            let planned = self.plan_run(&state.config, session_id);

            // Refuse before mutating anything: a session that cannot be
            // started has not "tried to start".
            let spec = start_spec(&state.config, state.terminal_size, session_id, &planned)?;

            state.runtime.status = SessionStatus::Starting;
            state.runtime.last_error = None;
            (spec, planned)
        };
        drop(sessions);
        // The listener sees the start in flight, which is what a UI needs to
        // keep the action buttons from lying about what is happening.
        self.publish_state_and_summary(session_id);

        let spawned = match spec {
            StartSpec::Process(spec) => ManagedProcess::spawn(spec)
                .map(Spawned::Process)
                .map_err(|error| error.to_string()),
            StartSpec::Terminal(spec) => Pty::spawn(spec)
                .map(|pty| Spawned::Terminal(Arc::new(pty)))
                .map_err(|error| error.to_string()),
            StartSpec::Standalone(spec) => IndependentProcess::spawn(spec)
                .map(Spawned::Standalone)
                .map_err(|error| error.to_string()),
        };

        let outcome = {
            let mut state = lock(&handle);

            match spawned {
                Ok(spawned) => {
                    let pid = spawned.pid();
                    let started_at = planned.started_at;

                    let log = Arc::new(RunLogHandle::new(RunLog::start(
                        planned.plan.persistence.clone(),
                        self.limits,
                    )));

                    let record = RunRecord {
                        run_id: planned.run_id.clone(),
                        session_id: session_id.to_owned(),
                        started_at,
                        ended_at: None,
                        exit_code: None,
                        pid: Some(pid),
                        log_mode: state.config.logging.mode,
                        log_source: state.config.logging.source,
                        // Known for the modes that have a file from the start:
                        // a `capture` run's file exists now, and an `external`
                        // session is linked to the application's own log. The
                        // modes that decide at the end leave this to be filled
                        // in when the run ends.
                        log_file: planned.planned_log_file(&log),
                    };

                    // The generation marks this run; a watcher started for it
                    // must not be able to publish an exit against a later one.
                    state.generation += 1;
                    // The relay's offsets start over with the run they belong
                    // to; a view attached to the previous run must not be able
                    // to confuse its tail with this one's head.
                    let generation = state.generation;
                    state.relay.begin(generation);
                    state.record = Some(record);
                    state.log = Some(Arc::clone(&log));
                    state.log_problem = planned.plan.problem.clone();
                    let buffer = lock(&state.buffer).summary();
                    state.runtime.buffer = buffer;

                    state.runtime.pid = Some(pid);
                    state.runtime.run_id = Some(planned.run_id.clone());
                    state.runtime.started_at = Some(started_at);
                    state.runtime.exit_code = None;
                    state.runtime.status = SessionStatus::Running;
                    // The previous run's reading belongs to that run. This run
                    // has not been probed yet, and the first probe (the
                    // watcher's, a moment from now) is what will say anything
                    // about it (T08 §12).
                    state.runtime.health = None;

                    let started = match spawned {
                        Spawned::Process(run) => {
                            // Taken before the run goes into the registry: the
                            // pumping threads are started once this lock is
                            // released, and an untaken pipe would leave a
                            // chatty service blocked on its own output.
                            let output = planned
                                .plan
                                .persistence
                                .captures_output()
                                .then(|| run.take_output())
                                .flatten();
                            state.run = Some(Run::Process(Arc::new(run)));
                            state.runtime.pty_attached = false;
                            Started::Process {
                                output,
                                generation: state.generation,
                            }
                        }
                        Spawned::Standalone(run) => {
                            // Nothing is wired: no pipes to take (the
                            // application kept its console) and no terminal
                            // view to attach. What the session holds is the
                            // run, which is what lets it report the state, find
                            // the application's window and — when the user
                            // asked for management — stop it (#66).
                            state.run = Some(Run::Standalone(Arc::new(run)));
                            state.runtime.pty_attached = false;
                            Started::Standalone {
                                generation: state.generation,
                            }
                        }
                        Spawned::Terminal(pty) => {
                            // The reader is started and registered under this
                            // same lock: a stop arriving between the state
                            // update and the registration would find no reader
                            // to end, and the thread would then outlive the run
                            // it was reading for.
                            let stop = TerminalPump::stop_flag();
                            let reader = Arc::clone(&stop);
                            let core = self.clone();
                            let id = session_id.to_owned();
                            let generation = state.generation;
                            let read = Arc::clone(&pty);
                            let handle = std::thread::spawn(move || {
                                pump_terminal(core, id, generation, read, reader);
                            });
                            state.terminal_pump = Some(TerminalPump::new(stop, handle));
                            state.run = Some(Run::Terminal(Arc::clone(&pty)));
                            state.runtime.pty_attached = true;
                            Started::Terminal {
                                pty,
                                generation: state.generation,
                                initial: state.config.initial_command.clone(),
                            }
                        }
                    };

                    Ok((state.runtime.clone(), started))
                }
                Err(message) => {
                    state.runtime.status = SessionStatus::Error;
                    state.runtime.pid = None;
                    // A start that never got a run has no start time. Leaving
                    // the previous run's here would make `Error` show an
                    // uptime for a run that does not exist.
                    state.runtime.started_at = None;
                    state.runtime.run_id = None;
                    state.runtime.pty_attached = false;
                    state.runtime.health = None;
                    state.run = None;
                    state.record = None;
                    state.log = None;
                    state.log_problem = None;
                    state.runtime.last_error = Some(SessionErrorInfo {
                        operation: "start".to_owned(),
                        message: message.clone(),
                    });
                    Err(SessionError::failed(
                        session_id,
                        "start",
                        message,
                        Some(SessionStatus::Starting),
                    ))
                }
            }
        };

        // Published after the session lock is released, so a sink can never
        // deadlock against the operation that produced the event.
        match outcome {
            Ok((runtime, started)) => {
                // The run's output is wired before anything is published or
                // watched: a run can exit instantly, and a watcher that got
                // there first would close the log before the pipes (or the
                // terminal) that feed it were even handed over — losing the
                // whole of a short run's output.
                let generation = started.generation();
                match started {
                    Started::Process { output, .. } => {
                        self.start_capturing(session_id, generation, output);
                    }
                    Started::Terminal { pty, initial, .. } => {
                        self.type_initial_command(session_id, &pty, initial.as_deref());
                    }
                    Started::Standalone { .. } => {}
                }
                self.publish_ending(session_id);

                // Started only after `Running` is on the wire. A run can end very
                // quickly, and a watcher that got there first would publish
                // `Exited` before `Running` — a listener would then be told a
                // session ended before it was told it started.
                std::thread::spawn({
                    let core = self.clone();
                    let handle = Arc::clone(&handle);
                    let session_id = session_id.to_owned();
                    move || watch_run(core, handle, session_id, generation)
                });
                Ok(runtime)
            }
            Err(error) => {
                self.publish_state_and_summary(session_id);
                Err(error)
            }
        }
    }

    /// Read a run's captured streams into its session, one thread per stream.
    ///
    /// The threads end when the pipes do — for a supervised process, when it
    /// exits and every handle it handed on is closed — so nothing has to stop
    /// them, and nothing has to wait for them: a run's last output arrives
    /// after the run is over, and the session it belongs to is asked for by id
    /// rather than held.
    fn start_capturing(
        &self,
        session_id: &str,
        generation: u64,
        output: Option<crate::process::ProcessOutput>,
    ) {
        let Some(output) = output else {
            return;
        };
        let sink = CapturedOutput {
            core: self.clone(),
            session_id: session_id.to_owned(),
            generation,
        };

        let (drained, done) = std::sync::mpsc::channel();
        let mut started = 0;
        for (reader, stream) in [
            (output.stdout, Stream::Stdout),
            (output.stderr, Stream::Stderr),
        ] {
            if let Some(reader) = reader {
                started += 1;
                let signalled = drained.clone();
                logging::pump(reader, stream, sink.clone(), move || {
                    let _ = signalled.send(());
                });
            }
        }
        drop(drained);

        if started > 0 {
            if let Some(handle) = self.handle(session_id) {
                lock(&handle).pump_drain = Some(PumpDrain {
                    done,
                    expected: started,
                });
            }
        }
    }

    /// Type a terminal's configured `initial_command` into it.
    ///
    /// Sent as input rather than assembled onto the shell's command line: it
    /// works for every shell without the session layer having to learn each
    /// one's "run this, then stay interactive" flag, and it is what actually
    /// happens — the command appears in the terminal as if it had been typed,
    /// which is also why the screen afterwards is explainable.
    ///
    /// A failure here is not a failed start: the terminal is up and the user
    /// can type. It is recorded as the session's last error, where it sits next
    /// to a running terminal that plainly did not run the command.
    fn type_initial_command(&self, session_id: &str, pty: &Pty, command: Option<&str>) {
        let Some(command) = command.map(str::trim).filter(|line| !line.is_empty()) else {
            return;
        };
        let Some(handle) = self.handle(session_id) else {
            return;
        };

        // Enter is carried as a carriage return, which is the byte a terminal
        // sends for the Enter key (`crate::pty`).
        let mut line = command.as_bytes().to_vec();
        line.push(b'\r');

        if let Err(error) = pty.write(&line) {
            lock(&handle).runtime.last_error = Some(SessionErrorInfo {
                operation: "start".to_owned(),
                message: format!(
                    "session `{session_id}` started, but its `initial_command` could not be \
                     sent to the terminal: {error}"
                ),
            });
        }
    }

    /// Take one batch of a run's output.
    ///
    /// Reached from a pumping thread — a service's pipe reader or a terminal's
    /// [`pump_terminal`] — never from a lifecycle call. The generation check is
    /// what keeps a superseded run's last bytes out of the session's buffer:
    /// after a restart, output that was already in the pipe belongs to the run
    /// that produced it, and showing it as the new run's output would be a lie
    /// the user cannot detect.
    ///
    /// The bytes go to three places, in this order, and the order is the
    /// decision D-004 records: the run's log first, then the session's
    /// scrollback, then the UI. A batch that reached the buffer but not the
    /// file would be a scrollback a user can read but cannot recover (memory
    /// is replenishable; a lost log is not), and a batch the UI missed is
    /// recoverable from the scrollback on the next attach.
    fn take_output_batch(&self, session_id: &str, generation: u64, stream: Stream, bytes: &[u8]) {
        let batch = {
            let Some(handle) = self.handle(session_id) else {
                return;
            };
            let mut state = lock(&handle);
            if state.generation != generation {
                return;
            }

            if let Some(log) = state.log.as_ref() {
                log.append(stream, bytes);
            }
            state.runtime.buffer = {
                let mut buffer = lock(&state.buffer);
                buffer.push(stream, bytes);
                buffer.summary()
            };

            // Published after the lock is released: the relay advances the
            // offset an attachment reads at the same moment the scrollback
            // takes these bytes, and a sink must not be able to deadlock
            // against the operation that produced its event.
            state.relay.push(generation, bytes, Instant::now())
        };

        if let Some(batch) = batch {
            self.publish_output(session_id, &batch);
        }
    }

    /// Publish whatever of a session's output is due.
    ///
    /// `force` publishes bytes that have not yet reached their batch window,
    /// which is what a run ending needs: the last lines of a session must not
    /// wait for a window nothing will ever open again.
    ///
    /// A no-op for a generation that is no longer current — checkable, since a
    /// reader thread can outlive the run it was reading for by a few bytes.
    fn flush_output(&self, session_id: &str, generation: u64, force: bool) {
        let Some(handle) = self.handle(session_id) else {
            return;
        };

        let batches = {
            let mut state = lock(&handle);
            if state.generation != generation {
                return;
            }
            state.relay.drain(Instant::now(), force)
        };

        for batch in &batches {
            self.publish_output(session_id, batch);
        }
    }

    /// Announce one batch of a run's output to the listeners.
    fn publish_output(&self, session_id: &str, batch: &OutputBatch) {
        self.sink
            .publish(SessionEvent::TerminalOutput(TerminalOutput::from_batch(
                session_id, batch,
            )));
    }

    /// Stop a session, giving it the default grace period to unwind.
    pub fn stop(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        self.stop_with_timeout(session_id, crate::process::DEFAULT_STOP_TIMEOUT)
    }

    /// Stop a session, waiting at most `timeout` for a graceful exit before
    /// escalating to the force path.
    ///
    /// The state a stopped session lands in reports what actually happened:
    /// a run that answered the request ends `Stopped`, one that had to be
    /// terminated ends `Exited`. That is T03's `StopOutcome` decision, not a
    /// second guess made here.
    pub fn stop_with_timeout(
        &self,
        session_id: &str,
        timeout: std::time::Duration,
    ) -> Result<SessionRuntime, SessionError> {
        self.end_run(session_id, "stop", Some(timeout))
    }

    /// Terminate a session's run without waiting for it to unwind.
    pub fn force_stop(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        self.end_run(session_id, "force_stop", None)
    }

    /// Replace a session's run with a fresh one, starting if it is not running.
    ///
    /// Spec §5 rule 2 — a restart must not launch a replacement until the
    /// previous managed process is confirmed stopped — is satisfied by
    /// construction rather than by a check here: the `stop` this calls is
    /// T03's stop barrier, which only returns `Ok` once the managed tree is
    /// observed gone. Starting first and hoping would be exactly the duplicate
    /// instance the rule exists to prevent.
    ///
    /// A session in `Starting` or `Stopping` is refused rather than queued:
    /// neither state can be interrupted, so there is no honest way to restart
    /// mid-flight.
    pub fn restart(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        // A restart is a stop and a start, so it is owed the same refusal a
        // stop is: the Hub cannot end a run it does not own (#66).
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, "restart"))?;
        // Nothing to restart: a restarting a standalone-window application
        // would end the copy the user already had and start the Hub's own,
        // which is the opposite of what association is for (#67).
        require_owned(&handle, session_id, "restart")?;
        require_managed(&handle, session_id, "restart")?;

        // `Error` may still own a live run its stop could not confirm ended
        // (#125). Starting over would drop that handle — and with it terminate
        // the tree — inside the start's lock, so the barrier comes first here
        // too: the same stop before the replacement every restart performs.
        let (status, live_run) = {
            let state = lock(&handle);
            (
                state.runtime.status,
                state.run.as_ref().is_some_and(Run::holds_live_process),
            )
        };
        let unsettled = status == SessionStatus::Error && live_run;
        if status == SessionStatus::Running || unsettled {
            // The barrier. When this returns, nothing of the old run is left.
            self.stop(session_id)?;
        }

        self.start(session_id)
    }

    /// This session's validated configuration, for a caller that has to reason
    /// about the entry before any lifecycle operation (#67).
    ///
    /// A copy rather than a reference: the session lock is released before the
    /// caller uses it, and a configuration cannot change under a running
    /// session anyway.
    pub fn session_config(&self, session_id: &str) -> Option<SessionConfig> {
        self.handle(session_id)
            .map(|handle| lock(&handle).config.clone())
    }

    /// Whether this session keeps the application's own window (#66).
    ///
    /// The display half of an entry's configuration, asked by id so a caller
    /// that has just activated a session can decide whether there is a window
    /// to bring forward without reading a listing.
    pub fn keeps_own_window(&self, session_id: &str) -> bool {
        self.handle(session_id)
            .map(|handle| lock(&handle).config.is_window())
            .unwrap_or(false)
    }

    /// Whether this session is associated with an instance the Hub did not
    /// start (#67).
    pub fn is_external(&self, session_id: &str) -> bool {
        self.handle(session_id)
            .map(|handle| lock(&handle).adopted.is_some())
            .unwrap_or(false)
    }

    /// Associate an application that was already running outside the Hub
    /// (#67).
    ///
    /// The caller has done the identifying — [`crate::app::external`] is where
    /// the evidence rules live — and what arrives here is the answer: an
    /// identity the Hub will check again before every later lookup, and a
    /// handle on the process itself, which is both what the ending is noticed
    /// through and the proof that the identity was verified when it was opened.
    ///
    /// ## Which states this may come from
    ///
    /// The same three a start may come from — `Stopped`, `Exited`, `Error` —
    /// and for the same reason: those are exactly the states that mean nothing
    /// of the Hub's is alive. What the session gets afterwards is `Running`,
    /// because the application really is.
    ///
    /// That move is deliberately not in [`SessionStatus::can_transition_to`]'s
    /// table. The table describes runs the Hub creates, and this creates none:
    /// nothing is spawned, no tree is owned, and no record is opened. What
    /// changes is only that the Hub now accounts for something already
    /// happening, which is a statement about the session rather than a move a
    /// lifecycle operation made.
    pub fn adopt(
        &self,
        session_id: &str,
        identity: ProcessIdentity,
        process: ExternalProcess,
    ) -> Result<SessionRuntime, SessionError> {
        const OPERATION: &str = "adopt";
        let sessions = lock(&self.sessions);
        let handle = sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        let generation = {
            let mut state = lock(&handle);
            let from = state.runtime.status;
            if !matches!(
                from,
                SessionStatus::Stopped | SessionStatus::Exited | SessionStatus::Error
            ) {
                return Err(SessionError::invalid_transition(
                    session_id,
                    OPERATION,
                    from,
                    SessionStatus::Running,
                ));
            }

            // The identity is checked once more here, against the process the
            // handle belongs to: between identifying it and this call the
            // caller did other work, and what it identified is a number
            // (decision 11).
            if !identity.matches() {
                return Err(SessionError::failed(
                    session_id,
                    OPERATION,
                    format!(
                        "process {} is no longer the one that was identified; Windows may have \
                         reused the number, so the Hub did not associate it",
                        identity.pid()
                    ),
                    Some(from),
                ));
            }

            state.generation += 1;
            let generation = state.generation;
            state.adopted = Some(Adopted {
                identity,
                process: Arc::new(process),
            });
            // A run the Hub did not start has no run id, no log and no record.
            // Its `started_at` is the process's own creation time, which is a
            // real answer to "how long has this been up" — and the honest one,
            // because the Hub's own clock started watching later.
            state.runtime.status = SessionStatus::Running;
            state.runtime.pid = Some(identity.pid());
            state.runtime.run_id = None;
            state.runtime.started_at = crate::process::started_unix_secs(identity.created_at())
                .and_then(Timestamp::from_unix_secs);
            state.runtime.exit_code = None;
            state.runtime.external = true;
            state.runtime.health = None;
            state.runtime.last_error = None;

            generation
        };

        drop(sessions);
        self.publish_state_and_summary(session_id);
        self.watch_adopted(session_id, &handle, generation);
        let runtime = lock(&handle).runtime.clone();
        Ok(runtime)
    }

    /// Forget an association, without touching the process (#67).
    ///
    /// This is the "明确新开" half of the choice: the user said the running
    /// instance is not the one they want this entry to be, so the Hub stops
    /// claiming it and starts its own. The instance itself is left exactly as
    /// it was — nothing here ends it, because the Hub never owned it — and the
    /// session goes back to the state it was in before, so the open that
    /// follows creates the Hub's own run.
    ///
    /// The session goes to `Stopped` rather than to `Exited`: an `Exited` here
    /// would claim a run of the Hub's had ended, and none ever existed.
    pub fn release_adopted(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        const OPERATION: &str = "release_adopted";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        {
            let mut state = lock(&handle);
            if state.adopted.take().is_none() {
                return Ok(state.runtime.clone());
            }
            // A new generation, so the watcher that was waiting on the process
            // the Hub has just stopped claiming cannot publish an ending for
            // it.
            state.generation += 1;
            state.runtime.status = SessionStatus::Stopped;
            state.runtime.pid = None;
            state.runtime.started_at = None;
            state.runtime.external = false;
            state.runtime.health = None;
        }
        self.publish_state_and_summary(session_id);
        let runtime = lock(&handle).runtime.clone();
        Ok(runtime)
    }

    /// Notice an associated instance ending on its own (#67).
    ///
    /// The same shape as [`watch_run`], and for the same reasons: the wait
    /// blocks on the process object so a tray-resident Hub holds no timer per
    /// instance, and an ending is published only for the generation it was
    /// started for — a release or a later start must not be told that the
    /// process it has stopped claiming has ended.
    fn watch_adopted(&self, session_id: &str, handle: &Arc<Mutex<SessionState>>, generation: u64) {
        let Some(process) = lock(handle)
            .adopted
            .as_ref()
            .map(|adopted| Arc::clone(&adopted.process))
        else {
            return;
        };

        let core = self.clone();
        let handle = Arc::clone(handle);
        let session_id = session_id.to_owned();
        let _ = std::thread::Builder::new()
            .name(format!("lch-external-{session_id}"))
            .spawn(move || {
                while !process.wait(WATCH_TICK) {
                    // Still alive: stop waiting if this watcher no longer owns
                    // the instance.
                    if lock(&handle).generation != generation {
                        return;
                    }
                }

                let mut state = lock(&handle);
                if state.generation != generation
                    || state.runtime.status != SessionStatus::Running
                    || state.adopted.is_none()
                {
                    return;
                }
                let code = process.exit_code();
                state.adopted = None;
                state.runtime.external = false;
                state.runtime.pid = None;
                state.runtime.started_at = None;
                state.runtime.status = SessionStatus::Exited;
                state.runtime.exit_code = code;
                drop(state);

                core.publish_state_and_summary(&session_id);
            });
    }

    /// The window this session's application presents, if it keeps its own
    /// (#66).
    ///
    /// The Hub's half of "重复打开唤起原窗口": opening an entry is
    /// [`SessionCore::activate`]'s business, and bringing the application's own
    /// window forward is a reading of the run that answer describes. A session
    /// the Hub hosts has no second window, so this answers `None` for it — and
    /// `None` is also the honest answer for a standalone application with no
    /// window on screen right now (spec #59 decision 11: report it, do not
    /// start another instance to make up for it).
    ///
    /// `timeout` is how long a just-started application is given to put its
    /// window up. A caller asking *for* the application — a click that means
    /// "show me that window" — passes the wait; a caller that is only reading
    /// passes zero and gets the current answer.
    pub fn application_window(
        &self,
        session_id: &str,
        timeout: std::time::Duration,
    ) -> Option<TopLevelWindow> {
        let (run, adopted) = match self.handle(session_id) {
            Some(handle) => {
                let state = lock(&handle);
                if !state.config.is_window() {
                    (None, None)
                } else {
                    (state.run.clone(), state.adopted.clone())
                }
            }
            None => (None, None),
        };

        // An instance the Hub associated is found the other way round: there is
        // no job object to ask for its tree, so the processes are read from the
        // table — and the console lookup is gated on the identity still
        // holding, because that lookup is by number (#67).
        if let Some(adopted) = adopted {
            let lead = adopted.identity.pid();
            return crate::window::wait_for_application_window(
                || {
                    let mut pids = vec![lead];
                    pids.extend(crate::process::descendants(lead));
                    crate::window::Processes {
                        pids,
                        lead,
                        lead_is_current: adopted.is_current(),
                    }
                },
                timeout,
            );
        }

        run?.application_window(timeout)
    }

    /// Open a session: start it if nothing is running, otherwise answer with
    /// the run it already has (#64, spec #59 decision 10).
    ///
    /// This is the single operation behind every "open this configured
    /// application" entry — the window's start control, and the launch request
    /// a later invocation hands to the Hub — so no two entries can disagree
    /// about what opening means, and none of them can create a second run by
    /// asking twice (user stories 33–34).
    ///
    /// It deliberately does not restart. Clicking an entry is not a request to
    /// interrupt what is already running; that is [`SessionCore::restart`], and
    /// a control that meant both would be the "点击打开被解释为重启" the spec
    /// forbids.
    ///
    /// A session caught mid-stop is refused rather than queued: `Stopping`
    /// cannot be interrupted (spec §5), so opening now would be exactly the
    /// parallel run the stop barrier exists to prevent. The caller gets the
    /// reason and the session keeps its own transition.
    ///
    /// ## Why this re-reads instead of deciding once
    ///
    /// The question ("is anything running?") and the claim ([`SessionCore::start`]
    /// setting `Starting`) are two separate acquisitions of the session lock,
    /// so two opens that arrive together — the window's control and a launch
    /// request handed to the Hub, on their own threads — can both see a stopped
    /// session. Only one of them then wins the claim; the loser is *not* an
    /// error, because the run it was about to create is the run the winner is
    /// already creating. It reads the session again and answers with that one,
    /// which is what "运行或启动中重复打开选中同一运行" has to mean when the
    /// two opens really are simultaneous.
    pub fn activate(&self, session_id: &str) -> Result<Activation, SessionError> {
        const OPERATION: &str = "activate";
        // Bounded rather than `loop`: each retry means another thread claimed
        // this session's start in the window between the read and the claim,
        // and a session cannot keep being claimed forever without one of those
        // starts ending. The bound keeps that reasoning from being the only
        // thing standing between a caller and a hang.
        const ATTEMPTS: usize = 4;

        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        for attempt in 0..ATTEMPTS {
            let (status, runtime) = {
                let state = lock(&handle);
                (state.runtime.status, state.runtime.clone())
            };

            match status {
                // Already open — running, or starting and about to be. The
                // run in flight is the answer; asking again changes nothing.
                SessionStatus::Starting | SessionStatus::Running => {
                    return Ok(Activation {
                        runtime,
                        started: false,
                    })
                }
                // Stopping is the one state an open must not interrupt: the
                // run going away has not gone away yet.
                SessionStatus::Stopping => {
                    return Err(SessionError {
                        kind: SessionErrorKind::InvalidTransition,
                        session_id: session_id.to_owned(),
                        operation: OPERATION.to_owned(),
                        message: format!(
                            "session `{session_id}` is stopping; opening it now would start a \
                             second run before the previous one is gone — wait for it to end, \
                             then open it again"
                        ),
                        from: Some(SessionStatus::Stopping),
                    })
                }
                // Stopped, Exited and Error all mean nothing is running, and
                // all three may start (spec §5).
                SessionStatus::Stopped | SessionStatus::Exited | SessionStatus::Error => {
                    match self.start(session_id) {
                        Ok(runtime) => {
                            return Ok(Activation {
                                runtime,
                                started: true,
                            })
                        }
                        // Someone claimed `Starting` between the read and the
                        // claim. Their start is the open this call asked for.
                        Err(error)
                            if error.kind == SessionErrorKind::InvalidTransition
                                && error.from == Some(SessionStatus::Starting)
                                && attempt + 1 < ATTEMPTS =>
                        {
                            continue
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        // Unreachable in practice: the last attempt reports the claim that beat
        // it rather than retrying past the bound.
        Err(SessionError::failed(
            session_id,
            OPERATION,
            "the session kept being claimed by another start; try opening it again",
            None,
        ))
    }

    fn end_run(
        &self,
        session_id: &str,
        operation: &'static str,
        timeout: Option<std::time::Duration>,
    ) -> Result<SessionRuntime, SessionError> {
        use crate::process::StopOutcome;

        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, operation))?;
        // An instance the Hub did not start is not one it may end (#67), and
        // that question comes first: an associated application's own
        // configuration may well say the Hub manages its lifecycle, which is
        // exactly the case where answering the second question alone would let
        // a stop through (decision 11).
        require_owned(&handle, session_id, operation)?;
        // A run the Hub does not own is not one it may end (#66).
        require_managed(&handle, session_id, operation)?;

        // Claim `Stopping` under the lock, then release it: the wait itself is
        // the slow part, and `session` is the only session it may hold up.
        //
        // `Stopping` is published here, before the request and the wait, rather
        // than once at the end. DEVELOPMENT §6 lists the UI update as a later
        // step, and this deliberately departs from that order — the departure
        // is recorded in D-015. What §6 requires of the ordering is unchanged:
        // request gracefully, wait, escalate against the managed tree, and only
        // report a terminal state once the tree is confirmed gone.
        let run = {
            let mut state = lock(&handle);

            let from = state.runtime.status;
            if !from.can_transition_to(SessionStatus::Stopping) {
                return Err(SessionError::invalid_transition(
                    session_id,
                    operation,
                    from,
                    SessionStatus::Stopping,
                ));
            }

            state.runtime.status = SessionStatus::Stopping;
            state.run.clone()
        };
        self.publish_state_and_summary(session_id);

        // A `Running` session with no run should be impossible; if the state
        // machine ever allowed it, saying so beats pretending a stop happened.
        let Some(run) = run else {
            let message = format!("session `{session_id}` owns no run to stop");
            let mut state = lock(&handle);
            state.runtime.status = SessionStatus::Error;
            // The snapshot reports the error, so it has to carry the reason
            // for it: §4 keeps a last structured error precisely so a failure
            // is explainable without a log.
            state.runtime.last_error = Some(SessionErrorInfo {
                operation: operation.to_owned(),
                message: message.clone(),
            });
            drop(state);
            self.publish_state_and_summary(session_id);
            return Err(SessionError::failed(
                session_id,
                operation,
                message,
                Some(SessionStatus::Stopping),
            ));
        };

        let report = match timeout {
            Some(timeout) => run.stop(timeout),
            None => run.force_stop(),
        };

        let outcome = {
            let mut state = lock(&handle);
            match report {
                Ok(report) => {
                    let ended = if report.outcome == StopOutcome::Exited {
                        SessionStatus::Stopped
                    } else {
                        SessionStatus::Exited
                    };
                    state.close_run(ended, report.exit.code);
                    Ok(state.runtime.clone())
                }
                Err(error) => {
                    let message = error.to_string();
                    state.runtime.status = SessionStatus::Error;
                    state.runtime.last_error = Some(SessionErrorInfo {
                        operation: operation.to_owned(),
                        message: message.clone(),
                    });
                    Err(SessionError::failed(
                        session_id,
                        operation,
                        message,
                        Some(SessionStatus::Stopping),
                    ))
                }
            }
        };

        // The run ended, so its record closed with it and has to be announced
        // alongside the state. A stop that failed left the record open and has
        // nothing new to say about it.
        match &outcome {
            Ok(_) => {
                // The user asked for this stop and it was carried out. Whatever
                // exit code the terminated process reported is an answer to
                // being stopped, not a failure (`RunEnding`).
                self.finalize_run(session_id, &handle, RunEnding::Requested);
                self.publish_ending(session_id);
            }
            Err(_) => {
                // The stop could not confirm the tree was gone, so the run may
                // still be alive — but this session is not waiting on it any
                // more, and an `on_error` log that never gets written because a
                // stop failed is the one case where the log matters most.
                self.finalize_run(session_id, &handle, RunEnding::Failed);
                self.publish_state_and_summary(session_id);
            }
        }
        outcome
    }

    /// Create, register and start a temporary interactive terminal (#62).
    ///
    /// The whole of the quick entry's backend: one call that answers with the
    /// new session's configuration and its real state, so the window can select
    /// and focus it without a second read (spec #59 decision 4). What it does
    /// *not* do is invent a second kind of session — the terminal it makes is
    /// registered, started, watched, stopped and closed by exactly the paths
    /// every other session uses, which is what keeps `#61`'s process-tree rules
    /// and T07's terminal contract applying to it unchanged.
    ///
    /// Nothing is written to the config file (decision 6), and nothing is
    /// written *about* it: a temporary terminal's logging is `off`/`none`, so
    /// its output lives in the bounded scrollback and nowhere else
    /// (`docs/LOGGING.md` §3, #59 decision 16).
    pub fn create_temporary_terminal(
        &self,
        cwd: Option<&str>,
    ) -> Result<CreatedSession, SessionError> {
        self.create_temporary_terminal_with(cwd, &temporary::SystemLookup, dirs::home_dir())
    }

    /// The same operation with the machine's answers injected.
    ///
    /// Which shells exist and where the user's home directory is are facts
    /// about the machine, not decisions of this layer (`super::temporary`), so
    /// the operation takes them as parameters: a test can then assert the
    /// preference order and the directory rule instead of asserting whatever
    /// is installed on the machine running it.
    pub fn create_temporary_terminal_with(
        &self,
        cwd: Option<&str>,
        lookup: &dyn ShellLookup,
        home: Option<PathBuf>,
    ) -> Result<CreatedSession, SessionError> {
        const OPERATION: &str = "create_terminal";

        // Minted first so every refusal below can name the terminal it is
        // about, and so the id it would have had is visible in the error.
        let identity = temporary::mint();
        let shell = temporary::resolve_shell(lookup)
            .map_err(|reason| SessionError::failed(&identity.id, OPERATION, reason, None))?;
        let cwd = temporary::resolve_cwd(cwd, home)
            .map_err(|reason| SessionError::failed(&identity.id, OPERATION, reason, None))?;

        let mut config = SessionConfig {
            id: identity.id,
            name: identity.name,
            session_type: SessionType::Terminal,
            cwd: Some(cwd),
            command: None,
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            // The resolved program, quoted if it has a space in it: the
            // config's `shell` is a command line, and `C:\Program Files\...`
            // is the normal case for PowerShell 7 rather than an exotic one.
            shell: Some(temporary::shell_command(&shell.program)),
            initial_command: None,
            // A temporary terminal is the Hub's own console by definition
            // (#62): it exists to be typed into in this window.
            display: DisplayMode::Internal,
            lifecycle: LifecycleOwner::Managed,
            // `off`/`none` rather than `auto`: `auto` exists to *choose* a
            // policy, and for a temporary shell every choice it could make
            // would be a file the user did not ask for (decision 16).
            logging: EffectiveLogging {
                mode: EffectiveLogMode::Off,
                source: LogSource::None,
                external_path: None,
            },
        };

        self.register_temporary(&mut config)?;
        // Announced before the start, never after it: every lifecycle event
        // the start publishes then belongs to a session listeners already know,
        // in the order the events were published (§9). An announcement sent
        // afterwards would race the run's own events — a shell that exits
        // immediately could have its `Exited` published before its creation,
        // and a listener told about a creation last would render it running.
        self.publish_created(&config.id);

        match self.start(&config.id) {
            Ok(runtime) => Ok(CreatedSession { config, runtime }),
            Err(error) => {
                // A terminal that could not start is not a terminal the user
                // has: the row is withdrawn rather than left behind as a
                // failure to explain (spec #59 decision 4 — "失败不留假运行
                // 项"). The start's own error is the answer.
                let _ = self.remove_session(&config.id);
                Err(error)
            }
        }
    }

    /// Take back a registration that never became the user's (#64).
    ///
    /// It exists for one caller and one reason: adding an application
    /// registers the session *before* the config file is written, so a
    /// duplicate id is refused while nothing has been written at all — and a
    /// write that then fails has to undo that registration, or the window
    /// would list an application the next start would not have (spec #59
    /// decision 15: 失败不虚报或留幽灵项).
    ///
    /// Silent on purpose. Nothing announced this session — the caller
    /// publishes it only once the save succeeded — so there is no listener to
    /// tell, and a `Removed` for an id nobody was told about would be an event
    /// about nothing.
    ///
    /// A session holding a run is never taken back: dropping its handle is the
    /// "close without settling ownership" #61 forbids. That cannot happen to
    /// this caller (a registration never has a run), and the check is here so
    /// it stays that way.
    pub(crate) fn unregister(&self, session_id: &str) -> bool {
        let mut sessions = lock(&self.sessions);
        let Some(state) = sessions.get(session_id) else {
            return false;
        };
        if lock(state).run.is_some() {
            return false;
        }
        sessions.remove(session_id);
        true
    }

    /// Record that this session's configuration is now one the config file
    /// describes (#65).
    ///
    /// Provenance and configuration move together, in one lock hold: the
    /// session stops being one the window made and becomes one the file
    /// describes — `temporary: false`, so it is no longer removable from the
    /// list and no longer lost when the app exits, and carrying the name the
    /// user saved it under rather than the one it was minted with.
    ///
    /// **Nothing about the run is touched.** The session keeps its id, its
    /// handle, its process tree and its scrollback: saving a terminal is a
    /// statement about where its launch configuration lives, and stating it
    /// must not create a second process, restart the first, or replay anything
    /// the user typed (spec #59 decision 6, story 25). The id in particular:
    /// it is what a terminal attachment, a deep link and a log directory are
    /// keyed by, and minting a prettier one would mean re-keying a live run.
    ///
    /// A session that is already configured is refused rather than re-marked.
    /// The caller has nothing new to say about it, and a second save would
    /// quietly rewrite what the file already holds under a name the user may
    /// have chosen there deliberately.
    pub(crate) fn mark_saved(
        &self,
        session_id: &str,
        config: SessionConfig,
    ) -> Result<SessionEntry, SessionError> {
        const OPERATION: &str = "mark_saved";

        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        let mut state = lock(&handle);
        if !state.temporary {
            return Err(SessionError::failed(
                session_id,
                OPERATION,
                format!(
                    "session `{session_id}` is already a configuration saved in the config file"
                ),
                None,
            ));
        }
        if config.id != session_id {
            // Unreachable from the one caller: the entry it validates names the
            // session it is saving. Refused rather than asserted, because the
            // alternative to saying so is a registry whose key and whose
            // configuration disagree about what this session is called.
            return Err(SessionError::failed(
                session_id,
                OPERATION,
                format!(
                    "the saved configuration names `{}` rather than `{session_id}`; a save has to \
                     describe the session it was made for",
                    config.id
                ),
                None,
            ));
        }

        state.config = config;
        state.temporary = false;
        Ok(SessionEntry {
            config: state.config.clone(),
            temporary: false,
        })
    }

    /// Remove a temporary session from the registry (#62).
    ///
    /// Refused for a configured session — that one belongs to the config file,
    /// and the window is not the place it is edited — and refused for anything
    /// still owning a run: removing such a session would drop the only handle
    /// accounting for its process tree, which is the same reason
    /// [`SessionCore::register`] refuses a duplicate id, and is what
    /// [`removable`] spells out.
    ///
    /// What a removal actually frees is the session's retained scrollback, so
    /// the offer is the ended one: "回看本次输出，然后把它从列表里去掉"
    /// (story 22).
    pub fn remove_session(&self, session_id: &str) -> Result<(), SessionError> {
        const OPERATION: &str = "remove_session";

        let mut sessions = lock(&self.sessions);
        let Some(state) = sessions.get(session_id).cloned() else {
            return Err(SessionError::unknown_session(session_id, OPERATION));
        };
        {
            // Registry lock, then session lock: the order every other path
            // takes, and the one that keeps a concurrent start from slipping
            // between the question and the removal.
            let state = lock(&state);
            if !state.temporary {
                return Err(SessionError::failed(
                    session_id,
                    OPERATION,
                    format!(
                        "session `{session_id}` is configured in the config file; remove it there \
                         rather than from the window"
                    ),
                    None,
                ));
            }
            if !removable(state.temporary, state.runtime.status, state.run.is_some()) {
                let message = match state.runtime.status {
                    // The one refusal that is not "wait for it to stop": this
                    // session's stop could not confirm its tree was gone, so
                    // the handle it still holds is the only thing accounting
                    // for that tree (§61's rule, upheld by #62).
                    SessionStatus::Error => format!(
                        "session `{session_id}` could not confirm that its process tree ended; \
                         stop or force stop it before removing it"
                    ),
                    status => format!(
                        "session `{session_id}` is {}; stop it before removing it",
                        status.as_str()
                    ),
                };
                return Err(SessionError::failed(
                    session_id,
                    OPERATION,
                    message,
                    Some(state.runtime.status),
                ));
            }
        }
        let removed = sessions.remove(session_id);
        // The registry lock is released before anything that can wait: a
        // `Pty` or `ManagedProcess` destructor runs the stop ladder, and every
        // other session's operations need this lock. The local `state` clone is
        // what keeps the `SessionState` — and those handles inside it — alive
        // past the guard, so it is dropped last, also outside the lock.
        drop(sessions);
        drop(removed);
        drop(state);

        self.publish_removed(session_id);
        Ok(())
    }

    /// Persist removal while holding both locks: a concurrent start cannot
    /// claim this entry while its saved configuration is being removed.
    pub fn remove_application(
        &self,
        session_id: &str,
        path: &std::path::Path,
    ) -> Result<(), SessionError> {
        const OPERATION: &str = "remove_application";
        let mut sessions = lock(&self.sessions);
        let handle = sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;
        let state = lock(&handle);
        if state.temporary
            || state.adopted.is_some()
            || !removable(true, state.runtime.status, state.run.is_some())
        {
            return Err(SessionError::failed(
                session_id,
                OPERATION,
                "请先停止应用；独立窗口应用请先在自己的窗口中退出。".to_owned(),
                Some(state.runtime.status),
            ));
        }
        crate::config::remove_saved_session(path, session_id).map_err(|error| {
            SessionError::failed(
                session_id,
                OPERATION,
                error.message(path),
                Some(state.runtime.status),
            )
        })?;
        sessions.remove(session_id);
        drop(state);
        drop(sessions);
        self.publish_removed(session_id);
        Ok(())
    }

    /// The effective logging state of one session (`docs/LOGGING.md` §1.4).
    ///
    /// Answered whether or not a run exists: "is this being recorded, and
    /// where?" is a property of the configuration, and a UI that can only
    /// answer it for running sessions would have to guess the rest of the time.
    pub fn log_status(&self, session_id: &str) -> Option<LogStatus> {
        let handle = self.handle(session_id)?;
        let state = lock(&handle);
        Some(state.log_status(session_id, self.roots.as_ref()))
    }

    /// A session's retained scrollback, oldest first.
    ///
    /// Read on demand rather than carried in the snapshot: a snapshot is copied
    /// into every event, and a full scrollback must not make every state change
    /// expensive (spec §14). The frontend reads this when it needs to render a
    /// terminal, not on every tick.
    pub fn terminal_buffer(&self, session_id: &str) -> Option<Vec<logging::BufferedChunk>> {
        let handle = self.handle(session_id)?;
        let state = lock(&handle);
        let buffer = lock(&state.buffer);
        Some(buffer.chunks().cloned().collect())
    }

    /// What a terminal view needs to start rendering a session's stream.
    ///
    /// A terminal view is created when the user selects a session, and again
    /// whenever it is shown after being hidden — but the session itself is not
    /// restarted by either, so the view has to be able to pick up a stream that
    /// is already in flight. This answers with the retained scrollback, the
    /// run's generation, and the byte offset the scrollback reaches: the view
    /// replays the chunks, then appends the batches whose `end` is beyond that
    /// offset and drops the ones it has already shown — append or drop, never
    /// splice.
    ///
    /// That rule only holds if no batch can *straddle* the offset, and the
    /// flush below is what buys it. Output taken but not yet published would
    /// otherwise be published later as a batch starting before this offset and
    /// ending after it, and a view that has already replayed those bytes would
    /// have to cut the batch in half. Publishing what is pending, in the same
    /// lock hold that reads the offset, makes the offset a batch boundary by
    /// construction: every later batch starts exactly where this one ended.
    ///
    /// Answering for a session that is not running is deliberate: the view
    /// renders the last run's scrollback under its "not running" state rather
    /// than an empty panel that pretends there is nothing to see.
    ///
    /// The scrollback is the *session's*, not the run's (`docs/LOGGING.md` §8
    /// keeps it across a restart), so after a restart the replay holds earlier
    /// runs' bytes as well and covers more than `emitted` — which counts this
    /// run's stream from zero. That is the intended reading, and it is why the
    /// rule above is about offsets into *one run's* stream rather than about
    /// how much text the view has rendered.
    pub fn terminal_attachment(
        &self,
        session_id: &str,
    ) -> Result<TerminalAttachment, SessionError> {
        const OPERATION: &str = "attach_terminal";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        // Cut, read and release under one lock (see the note above); the
        // batches go out once it is released, as every publish in this module
        // does.
        let (attached, batches) = {
            let mut state = lock(&handle);

            let batches = state.relay.drain(Instant::now(), true);

            let (chunks, summary) = {
                let buffer = lock(&state.buffer);
                let chunks = buffer.chunks().map(RetainedChunk::encode).collect();
                (chunks, buffer.summary())
            };

            let attached = TerminalAttachment {
                session_id: session_id.to_owned(),
                pty_attached: state.runtime.pty_attached,
                generation: state.relay.generation(),
                emitted: state.relay.emitted(),
                chunks,
                buffer: summary,
            };
            (attached, batches)
        };

        for batch in &batches {
            self.publish_output(session_id, batch);
        }
        Ok(attached)
    }

    /// Send input to a session's terminal.
    ///
    /// Keystrokes go to the shell exactly as a terminal would deliver them, so
    /// Ctrl+C is the byte `0x03` travelling this way rather than an operation
    /// of its own: it interrupts what the shell is running, and does not close
    /// the session (spec §7). Only an interactive terminal has this path — a
    /// supervised service in the MVP runs without an attached stdin, and saying
    /// so beats accepting bytes that would go nowhere.
    pub fn terminal_write(&self, session_id: &str, bytes: &[u8]) -> Result<(), SessionError> {
        const OPERATION: &str = "terminal_write";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        /// Where input could go for this session right now.
        enum Input {
            Pty(Arc<Pty>),
            /// A supervised run, which has no attached stdin in the MVP.
            Unattached,
            /// Nothing is running.
            Idle,
        }

        let (input, session_type) = {
            let state = lock(&handle);
            // A session accepts input while it is *running*: the handle alone
            // is not enough, because a session that has been stopped keeps it
            // (as a stopped service keeps its process handle) and the shell
            // behind it is gone.
            let input = match (&state.run, state.runtime.status) {
                (Some(Run::Terminal(pty)), SessionStatus::Running) => Input::Pty(Arc::clone(pty)),
                (Some(Run::Process(_)), _) => Input::Unattached,
                _ => Input::Idle,
            };
            (input, state.config.session_type)
        };

        match input {
            Input::Pty(pty) => pty.write(bytes).map_err(|error| {
                SessionError::failed(session_id, OPERATION, error.to_string(), None)
            }),
            Input::Unattached => Err(SessionError::unsupported(
                session_id,
                OPERATION,
                format!(
                    "session `{session_id}` runs as a supervised service, which has no \
                     attached stdin to type into"
                ),
            )),
            Input::Idle => Err(SessionError::failed(
                session_id,
                OPERATION,
                match session_type {
                    SessionType::Terminal => format!(
                        "session `{session_id}` has no running terminal; start it before \
                         typing into it"
                    ),
                    SessionType::Service => format!(
                        "session `{session_id}` is not running, and a service has no \
                         attached stdin to type into"
                    ),
                },
                None,
            )),
        }
    }

    /// Tell a session's terminal how large its view is.
    ///
    /// A live terminal is resized immediately. One that is not running
    /// *remembers* the size instead of refusing it, and that is the interesting
    /// half: a shell is started at the geometry its view has already measured,
    /// so its first screen is drawn at the right width rather than at the
    /// default and re-flowed a moment later. A resize is also not a user
    /// command that can fail — the view reports its own size whenever it
    /// changes — so a size arriving for a stopped session is not an error.
    pub fn terminal_resize(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        const OPERATION: &str = "terminal_resize";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        if cols == 0 || rows == 0 || cols > MAX_DIMENSION || rows > MAX_DIMENSION {
            return Err(SessionError::failed(
                session_id,
                OPERATION,
                format!(
                    "a terminal needs at least 1x1 and at most {MAX_DIMENSION}x{MAX_DIMENSION} \
                     cells, got {cols}x{rows}"
                ),
                None,
            ));
        }

        let live = {
            let mut state = lock(&handle);
            state.terminal_size = Some((cols, rows));
            state.run.as_ref().and_then(Run::as_terminal).cloned()
        };

        if let Some(pty) = live {
            pty.resize(cols, rows).map_err(|error| {
                SessionError::failed(session_id, OPERATION, error.to_string(), None)
            })?;
        }
        Ok(())
    }

    /// Every registered session's validated configuration, in id order.
    ///
    /// The window renders a session from its configuration (name, purpose,
    /// close impact, port, cwd) and its snapshot (state, pid, uptime); this is
    /// the first half, and it is the same set the snapshots come from — one
    /// registry, so a session the window lists is one Session Core can act on.
    pub fn configs(&self) -> Vec<SessionConfig> {
        self.entries()
            .into_iter()
            .map(|entry| entry.config)
            .collect()
    }

    /// The same list, each configuration with its provenance (#62).
    ///
    /// The tray and the bootstrap path read [`SessionCore::configs`]: neither
    /// has anything to say about where a session came from. The window does —
    /// it is the surface that offers "remove this one" — so the listing it
    /// reads says which sessions are temporary, from the same lock hold that
    /// gives it the configurations.
    pub fn entries(&self) -> Vec<SessionEntry> {
        lock(&self.sessions)
            .values()
            .map(|state| {
                let state = lock(state);
                SessionEntry {
                    config: state.config.clone(),
                    temporary: state.temporary,
                }
            })
            .collect()
    }

    /// One session's configuration together with its provenance.
    ///
    /// The pair travel together because both halves of an answer need both:
    /// a listing that marks which rows are temporary, and the creation event
    /// that files one under the same rule.
    pub(crate) fn session_entry(&self, session_id: &str) -> Option<SessionEntry> {
        self.handle(session_id).map(|state| {
            let state = lock(&state);
            SessionEntry {
                config: state.config.clone(),
                temporary: state.temporary,
            }
        })
    }

    /// The URL "open this session's page" should hand to the OS (spec §9).
    ///
    /// Resolved here rather than passed in for the same reason a log action is
    /// (`SessionCore::log_file_path`): the frontend names a session, and the
    /// layer holding that session's configuration is the one that knows which
    /// URL it owns. The alternative — a command that opened whatever string it
    /// was given — would hand the webview a general launcher for any URL, which
    /// is not a capability a control surface needs (`crate::shell`'s note).
    ///
    /// The URL was validated when the config was read (`config::validate_url`:
    /// http/https only), so what comes back is already known to be a URL the OS
    /// should be asked to open rather than, say, a `file:` path.
    pub fn session_url(&self, session_id: &str) -> Result<String, SessionError> {
        const OPERATION: &str = "open_session_url";
        let config = self.config_of(session_id, OPERATION)?;
        config.url.ok_or_else(|| {
            SessionError::unsupported(
                session_id,
                OPERATION,
                format!(
                    "session `{session_id}` has no `url` in its configuration — add one to the \
                     session and this action can open it in the browser"
                ),
            )
        })
    }

    /// The folder "open this session's working directory" should hand to the OS
    /// (spec §9).
    ///
    /// Whether the folder is still *there* is not decided here: the config was
    /// checked when it was read and a user can delete a directory afterwards, so
    /// [`crate::shell::open_path`] is what reports a path that has since gone
    /// missing, with the advice that belongs to it.
    pub fn session_cwd(&self, session_id: &str) -> Result<PathBuf, SessionError> {
        const OPERATION: &str = "open_session_cwd";
        let config = self.config_of(session_id, OPERATION)?;
        config.cwd.ok_or_else(|| {
            SessionError::unsupported(
                session_id,
                OPERATION,
                format!(
                    "session `{session_id}` has no `cwd` in its configuration — set the folder it \
                     runs in and this action can open it"
                ),
            )
        })
    }

    /// One session's validated configuration, or the refusal that names the
    /// operation that wanted it.
    fn config_of(&self, session_id: &str, operation: &str) -> Result<SessionConfig, SessionError> {
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, operation))?;
        let config = lock(&handle).config.clone();
        Ok(config)
    }

    /// Commit the current run's `on_error` log now, instead of waiting for the
    /// run to end (`docs/LOGGING.md` §3: "save this run's log").
    ///
    /// Answers with the logging status afterwards, so a caller can show where
    /// the file went without a second round trip. A session with no run, or one
    /// whose policy does not write on request, is reported as the failed
    /// operation it is.
    pub fn save_run_log(&self, session_id: &str) -> Result<LogStatus, SessionError> {
        const OPERATION: &str = "save_run_log";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        {
            let state = lock(&handle);
            let log = state.log.as_ref().ok_or_else(|| {
                SessionError::failed(
                    session_id,
                    OPERATION,
                    format!("session `{session_id}` has no run to save a log for"),
                    None,
                )
            })?;
            log.save_now();
        }
        // A failed save is not a failed operation: the file may simply not be
        // writable, and the status carries that reason (`last_error`) where the
        // UI can show it.
        self.log_status(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))
    }

    /// Start or stop recording a `manual` session's run
    /// (`docs/LOGGING.md` §3).
    ///
    /// Refused for any other mode: `always` is already recording and `off` has
    /// no file to start, so accepting the request would make the UI's switch
    /// mean different things for different sessions.
    pub fn set_log_recording(
        &self,
        session_id: &str,
        recording: bool,
    ) -> Result<LogStatus, SessionError> {
        const OPERATION: &str = "set_log_recording";
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))?;

        {
            let state = lock(&handle);
            if state.config.logging.mode != crate::config::EffectiveLogMode::Manual {
                return Err(SessionError::failed(
                    session_id,
                    OPERATION,
                    format!(
                        "session `{session_id}` is not configured with `logging.mode: manual`, so \
                         recording cannot be switched on and off"
                    ),
                    None,
                ));
            }
            let log = state.log.as_ref().ok_or_else(|| {
                SessionError::failed(
                    session_id,
                    OPERATION,
                    format!("session `{session_id}` has no run to record"),
                    None,
                )
            })?;
            if recording {
                log.start_recording();
            } else {
                log.stop_recording();
            }
        }
        self.log_status(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, OPERATION))
    }

    /// Remove the Hub's own log files that retention says are past keeping
    /// (`docs/LOGGING.md` §9).
    ///
    /// The budget is per session, so a noisy service that reaches its own
    /// ceiling stops deleting its own oldest logs and never touches another
    /// session's. Passing a `session_id` narrows the sweep to that session —
    /// the "clean this session's old logs" action §10 lists — and `None`
    /// sweeps every session.
    ///
    /// Nothing is cleaned automatically, and nothing here deletes a file that
    /// is not the Hub's own: deleting a user's files is an action they take,
    /// not one that happens behind them when the app starts. The button that
    /// calls this is T10's, along with showing what a sweep would remove.
    pub fn cleanup_logs(&self, session_id: Option<&str>) -> logging::CleanupReport {
        let Some(roots) = &self.roots else {
            return logging::CleanupReport::default();
        };
        logging::retention::enforce(
            &roots.logs_dir,
            session_id,
            &logging::DEFAULT_RETENTION,
            logging::clock::now(),
            &self.application_logs(session_id),
        )
    }

    /// A session's completed runs, newest first (`docs/LOGGING.md` §6).
    ///
    /// Read from the run metadata on disk rather than from memory: a run
    /// history that only existed while the app was open would not survive the
    /// restart that a user is most likely to want it after.
    ///
    /// Each entry says whether its log is still on disk, because a sweep may
    /// have taken it since: [`SessionCore::cleanup_logs`] deletes log files and
    /// never the record of the run that wrote them, and the history is where
    /// that difference becomes visible rather than being discovered by a file
    /// action that fails.
    pub fn run_history(&self, session_id: &str) -> logging::RunHistory {
        let Some(roots) = &self.roots else {
            return logging::RunHistory::default();
        };
        logging::run_history(&roots.metadata_dir, session_id)
    }

    /// What a cleanup would remove, without removing anything
    /// (`docs/LOGGING.md` §10).
    ///
    /// The confirmation step before [`SessionCore::cleanup_logs`]: a
    /// destructive action says how many files and how many bytes it is about
    /// to take before it takes them.
    pub fn cleanup_preview(&self, session_id: Option<&str>) -> logging::CleanupReport {
        let Some(roots) = &self.roots else {
            return logging::CleanupReport::default();
        };
        logging::cleanup_preview(
            &roots.logs_dir,
            session_id,
            &logging::DEFAULT_RETENTION,
            logging::clock::now(),
            &self.application_logs(session_id),
        )
    }

    /// The application-owned log files of the sessions a sweep covers.
    ///
    /// `docs/LOGGING.md` §9 promises an application's log is never deleted, and
    /// `docs/DEVELOPMENT.md` §11 forbids deleting one unasked. The sweep keeps
    /// that promise by *identity* as well as by location: these paths are handed
    /// to it as protected, so a config that points an application's log into the
    /// Hub's own log tree — which nothing forbids — still cannot have it swept
    /// away with the Hub's files.
    fn application_logs(&self, session_id: Option<&str>) -> Vec<PathBuf> {
        let sessions = lock(&self.sessions);
        let mut paths = Vec::new();
        for (id, state) in sessions.iter() {
            if session_id.is_some_and(|wanted| wanted != id.as_str()) {
                continue;
            }
            if let Some(external) = lock(state).config.logging.external_path.clone() {
                paths.push(PathBuf::from(external));
            }
        }
        paths
    }

    /// The file "open this session's log" should hand to the OS
    /// (`docs/LOGGING.md` §10).
    ///
    /// Resolved here rather than handed in by the caller: the frontend can name
    /// a session and a run, and this is the layer that knows which file those
    /// two produced. The alternative — an IPC command that opens whatever path
    /// it is given — would make the window a general file launcher, which is
    /// not a capability a logs view needs (`crate::shell`'s module note).
    pub fn log_file_path(
        &self,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<PathBuf, SessionError> {
        const OPERATION: &str = "open_log_file";
        self.log_file_target(session_id, run_id, OPERATION)
    }

    /// The folder "open the containing folder" should hand to the OS.
    ///
    /// The same resolution as [`SessionCore::log_file_path`], followed by the
    /// folder that holds the file: a user asking where a log lives is asking
    /// about the folder, and it is still the right answer when the log itself
    /// has since been cleaned up.
    pub fn log_folder_path(
        &self,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<PathBuf, SessionError> {
        const OPERATION: &str = "open_log_folder";
        let file = self.log_file_target(session_id, run_id, OPERATION)?;
        file.parent()
            .map(|folder| folder.to_path_buf())
            .ok_or_else(|| {
                SessionError::failed(
                    session_id,
                    OPERATION,
                    format!("`{}` has no containing folder to open", file.display()),
                    None,
                )
            })
    }

    /// Resolve one log action to a file, or explain why there is none.
    ///
    /// The three answers, in order (`docs/LOGGING.md` §1.4 — the UI must never
    /// have to infer persistence from the presence of a file):
    ///
    /// 1. an `external` session points at the application's own log, whatever
    ///    run was asked for: the Hub linked that file rather than copying it
    ///    (D-005), and the application owns its own retention;
    /// 2. a named run resolves through *that run's* record, which is the only
    ///    place that knows which file the run produced;
    /// 3. no run named means the session's current answer: the file being
    ///    written now, or the one the last run left behind.
    fn log_file_target(
        &self,
        session_id: &str,
        run_id: Option<&str>,
        operation: &str,
    ) -> Result<PathBuf, SessionError> {
        let handle = self
            .handle(session_id)
            .ok_or_else(|| SessionError::unknown_session(session_id, operation))?;
        let (status, current_run) = {
            let state = lock(&handle);
            (
                state.log_status(session_id, self.roots.as_ref()),
                state.current_run_id(),
            )
        };

        // 1. The application's own log. Linked, never written by the Hub.
        //
        // `external_log` is copied out of the config, and the config layer is
        // what validates that a path only ever travels with `source: external`.
        // Core does not borrow that promise: it asks the source itself, so the
        // two cannot drift if a config reaches the registry another way.
        if status.source == LogSource::External {
            if let Some(external) = status.external_log {
                return Ok(PathBuf::from(external));
            }
        }

        // 2. A named run.
        if let Some(wanted) = run_id {
            // A run that is still in flight has no metadata on disk yet, and its
            // file is the one the status is already naming.
            if current_run.as_deref() == Some(wanted) {
                if let Some(path) = status.log_file {
                    return Ok(PathBuf::from(path));
                }
            }
            let history = self.run_history(session_id);
            let Some(entry) = history
                .runs
                .iter()
                .find(|entry| entry.run.run_id.as_str() == wanted)
            else {
                return Err(SessionError::failed(
                    session_id,
                    operation,
                    format!(
                        "session `{session_id}` has no run `{wanted}` in its history — the list \
                         may have been cleaned up, or the id may be from another session"
                    ),
                    None,
                ));
            };
            return match &entry.run.log_file {
                Some(path) => Ok(PathBuf::from(path)),
                None => Err(SessionError::failed(
                    session_id,
                    operation,
                    format!(
                        "run `{wanted}` of session `{session_id}` left no log file — {}",
                        policy_reason(&status)
                    ),
                    None,
                )),
            };
        }

        // 3. The session's current file.
        match status.log_file {
            Some(path) => Ok(PathBuf::from(path)),
            None => Err(SessionError::failed(
                session_id,
                operation,
                format!(
                    "session `{session_id}` has no log file to open — {}",
                    policy_reason(&status)
                ),
                None,
            )),
        }
    }

    /// Close the current run's log, finish its record, and file the record.
    ///
    /// Every path that ends a run calls this — a stop the user asked for, a
    /// failure to stop, and the watcher seeing a run end on its own — because a
    /// run whose log is never closed is a run whose output is still in a
    /// buffer, and one whose record is never filed is missing from the history
    /// the user will look at next time.
    fn finalize_run(&self, session_id: &str, handle: &Arc<Mutex<SessionState>>, ending: RunEnding) {
        // First, with the lock released: let the pipes be read out. A run that
        // exited with output still in flight has not finished producing it, and
        // closing its log now would file a file that is missing exactly the
        // lines a user opens it for.
        if let Some(drain) = lock(handle).pump_drain.take() {
            drain.wait();
        }

        // A terminal's reader is stopped rather than waited for. The console
        // host outlives the shell it hosted (`crate::pty`), so a reader that
        // only stopped when its stream ended would keep reading a terminal
        // that is over until the session's next run replaced it — an invisible
        // terminal still working, which spec §14 rules out. Stopping it takes
        // the last of the output with it: the reader flushes before it returns.
        //
        // Taken out of the lock before it is waited on, and not joined from
        // inside a guard: that flush needs this very lock, so waiting on the
        // thread with it held would wait forever.
        let pump = lock(handle).terminal_pump.take();
        if let Some(pump) = pump {
            pump.stop_and_wait();
        }

        // Anything taken but not yet published is due: the run is over, so no
        // batch window will open for these bytes again.
        {
            let generation = lock(handle).generation;
            self.flush_output(session_id, generation, true);
        }

        let record = {
            let mut state = lock(handle);

            if let Some(log) = state.log.take() {
                let exit_code = state.record.as_ref().and_then(|record| record.exit_code);
                let written = log.finish(ending.outcome(exit_code));

                if let Some(problem) = log.last_error() {
                    state.log_problem = Some(problem);
                }
                // Nothing was produced. An `external` session keeps its link to
                // the application's log — that file exists whether or not the
                // Hub wrote anything — and every other mode's record must stop
                // naming the file it planned but never wrote
                // (`docs/MVP_IMPLEMENTATION_SPEC.md` §4: a path *when one
                // exists*).
                let missing = (state.config.logging.source == LogSource::External)
                    .then(|| state.config.logging.external_path.clone())
                    .flatten();

                if let Some(record) = state.record.as_mut() {
                    record.log_file = written.map(|path| path.display().to_string()).or(missing);
                }
            }
            state.record.clone()
        };

        // Only a run that ended has a history entry. A record with no end is
        // an open run — a stop that could not confirm the tree was gone leaves
        // one — and filing it would put a run in the history that a reader
        // cannot tell from one still going.
        if let Some(record) = record.filter(|record| record.ended_at.is_some()) {
            self.file_run_record(session_id, &record);
        }
    }

    /// Write a finished run's record to the metadata directory
    /// (`docs/LOGGING.md` §6).
    ///
    /// A failure is recorded against the session rather than raised: the run
    /// itself ran fine, and a missing history entry must not be reported as a
    /// failed run. It is also not silent — it reaches the UI through the log
    /// status, next to the log file it is about.
    fn file_run_record(&self, session_id: &str, record: &RunRecord) {
        let Some(roots) = &self.roots else {
            return;
        };
        if let Err(error) =
            logging::write_run(&roots.metadata_dir, record, config::local_utc_offset_secs())
        {
            if let Some(handle) = self.handle(session_id) {
                lock(&handle).log_problem = Some(error);
            }
        }
    }

    /// Decide a run's logging before it starts.
    ///
    /// The run id and start time are minted here because they name the log
    /// file, and the file's path is what the policy is resolved against: a
    /// policy that needs a file the Hub cannot name is downgraded, and the
    /// reason travels with the run (`docs/LOGGING.md` §1.4).
    fn plan_run(&self, config: &SessionConfig, session_id: &str) -> PlannedRun {
        let run_id = RunId::mint();
        let started_at = Timestamp::now();

        let log_file = (config.logging.source == LogSource::Captured)
            .then(|| self.run_log_file(session_id, &run_id, started_at))
            .flatten();

        PlannedRun {
            plan: LogPlan::resolve(&config.logging, log_file),
            run_id,
            started_at,
        }
    }

    /// The path this run's log would live at, or `None` when the Hub has no
    /// writable root or the session id cannot become a directory name.
    fn run_log_file(
        &self,
        session_id: &str,
        run_id: &RunId,
        started_at: Timestamp,
    ) -> Option<PathBuf> {
        let roots = self.roots.as_ref()?;
        config::run_log_path(
            &roots.logs_dir,
            session_id,
            started_at.unix_secs()?,
            config::local_utc_offset_secs(),
            run_id.as_str(),
        )
    }
}

/// How a run ended, in the terms the logging layer decides with.
///
/// This is not [`SessionStatus`]: a run stopped by request and a run that fell
/// over both leave `Exited` behind when the exit code is zero, and `Error`
/// covers both an unexpected failure and a stop that could not be carried out.
/// The logging layer needs the difference — a stop the user asked for is not
/// evidence of a problem (`docs/LOGGING.md` §3) — so the caller that knows
/// says which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunEnding {
    /// The user stopped it, and the stop was carried out.
    Requested,
    /// It ended without anyone asking.
    OnItsOwn,
    /// The lifecycle called it a failure: an error state, or a stop that could
    /// not confirm the tree was gone.
    Failed,
}

impl RunEnding {
    /// The outcome `RunEnding` implies for a run that ended with `exit_code`.
    fn outcome(self, exit_code: Option<u32>) -> RunOutcome {
        match self {
            RunEnding::Requested => RunOutcome::stopped(exit_code),
            RunEnding::OnItsOwn => RunOutcome::exited(exit_code),
            RunEnding::Failed => RunOutcome::failed(exit_code),
        }
    }
}

/// Everything a run needs that is decided before it starts.
///
/// The run id and start time are minted here rather than after the spawn
/// because they are what name the run's log file (D-011): a run that starts and
/// immediately fails still has one well-defined file to look for, and whether
/// there is a file at all is settled once, before anything is launched.
struct PlannedRun {
    run_id: RunId,
    started_at: Timestamp,
    plan: LogPlan,
}

impl PlannedRun {
    /// Whether the Hub takes this run's stdout/stderr at all
    /// (`docs/LOGGING.md` §8: the scrollback is kept for every session; only
    /// `source: external` declines, because the application already has it).
    fn captures_output(&self) -> bool {
        self.plan.persistence.captures_output()
    }

    /// The `log_file` a run's record starts with, from what the run log
    /// actually has.
    ///
    /// Taken from the log itself rather than from the plan: a `capture` run
    /// whose file could not be created must not leave a record naming a path
    /// that does not exist (`docs/MVP_IMPLEMENTATION_SPEC.md` §4 — "persisted
    /// log path **when one exists**"). `external` is the other answer this can
    /// give, and it is a link to a file the application owns and already
    /// wrote. `on_error` and `manual` write nothing until something happens,
    /// so they leave this empty and it is filled in when the run ends — which
    /// is the difference between "there is a log" and "there may be one".
    fn planned_log_file(&self, log: &RunLogHandle) -> Option<String> {
        log.produced_path()
            .or_else(|| self.plan.persistence.external_path().cloned())
            .map(|path| path.display().to_string())
    }
}

/// Routes one run's captured batches to the session that owns it.
///
/// Holds a session id and a generation rather than a session handle: the run
/// may be superseded while its last output is still in the pipe, and the
/// generation is what lets the session refuse bytes that no longer belong to
/// the run it is showing.
#[derive(Clone)]
struct CapturedOutput {
    core: SessionCore,
    session_id: String,
    generation: u64,
}

impl OutputSink for CapturedOutput {
    fn take(&self, stream: Stream, bytes: &[u8]) {
        self.core
            .take_output_batch(&self.session_id, self.generation, stream, bytes);
    }
}

/// How long a run's end waits for its pipes to be read out.
///
/// A process can exit with output still in its pipe, and the run is not over
/// until that output has been taken (`docs/LOGGING.md` §3: an `on_error` run's
/// whole value is the output right before the failure). The wait is bounded
/// because a pipe is only closed when *every* writer lets go, and a service
/// that hands one to a child it outlives would otherwise hold the session
/// here for as long as that child lives.
///
/// The common case returns in microseconds — a pipe's worth of already-written
/// bytes — and the bound only matters for the case that has no answer anyway:
/// output from a run whose pipe is still held will reach the in-memory
/// scrollback, and a log file that has already been closed takes no more.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// The pumps reading one run's pipes, and how many of them there are.
struct PumpDrain {
    done: std::sync::mpsc::Receiver<()>,
    expected: usize,
}

impl PumpDrain {
    /// Wait, up to [`DRAIN_TIMEOUT`], for every pump to report that its stream
    /// ended.
    ///
    /// Must be called with no session lock held: the pumps need that lock to
    /// hand over their last batches, and waiting for them while holding it
    /// would be a deadlock that ends in the timeout — with exactly the output
    /// this exists to save missing.
    fn wait(self) {
        let deadline = std::time::Instant::now() + DRAIN_TIMEOUT;
        for _ in 0..self.expected {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if self.done.recv_timeout(left).is_err() {
                // Timed out or every sender is gone: either way there is
                // nothing more to wait for.
                return;
            }
        }
    }
}

impl SessionState {
    /// Record that the current run has ended, in the snapshot and in the run
    /// record together.
    ///
    /// One method rather than two updates at each call site: the snapshot is
    /// what the UI renders and the record is what run history keeps, and they
    /// describe the same event. Letting a caller update one and forget the
    /// other is how they drift.
    fn close_run(&mut self, status: SessionStatus, code: Option<u32>) {
        self.runtime.status = status;
        self.runtime.exit_code = code;
        self.runtime.pid = None;
        // Nothing is attached to a session that is not running. The terminal
        // handle stays where it is (as a stopped service's process handle
        // does) and is released when the next run replaces it; what must not
        // survive is the claim that a terminal view could type into it.
        self.runtime.pty_attached = false;
        // A reading describes a run in flight. Once there is none, the last one
        // is not a fact about now, and leaving it up would have the Details tab
        // report "not listening" for a service the user has just stopped — a
        // sentence about something being wrong rather than nothing being
        // checked (T08 §12).
        self.runtime.health = None;

        if let Some(record) = self.record.as_mut() {
            record.ended_at = Some(Timestamp::now());
            record.exit_code = code;
        }
    }

    /// Record a health reading taken for the run `generation` names.
    ///
    /// `true` when the snapshot changed and the reading is worth announcing.
    /// Two things answer `false`, and both are about the reading no longer
    /// being news: the session has moved past that run — a restart replaced it,
    /// or a stop has already cleared the reading — so writing it would report
    /// the old process's health as the new run's; or it is the reading already
    /// on the snapshot, which nothing needs republishing for.
    fn record_health(&mut self, generation: u64, reading: health::ServiceHealth) -> bool {
        if self.generation != generation || self.runtime.health == Some(reading) {
            return false;
        }
        self.runtime.health = Some(reading);
        true
    }

    /// The run this session's log answers for: the live one, or the last one
    /// that ran. `None` before the session has ever started.
    ///
    /// Paired with [`SessionState::log_status`]'s `log_file` — the file is the
    /// current run's while it is being written and the last run's afterwards,
    /// so the id that names it has to follow the same rule.
    fn current_run_id(&self) -> Option<String> {
        self.record
            .as_ref()
            .map(|record| record.run_id.as_str().to_owned())
    }

    /// The effective logging state of this session (`docs/LOGGING.md` §1.4).
    fn log_status(&self, session_id: &str, roots: Option<&LogRoots>) -> LogStatus {
        let logging = &self.config.logging;
        // While a run is in flight the run log answers "what is happening
        // now"; between runs the policy answers "what will happen next".
        // `manual` resolves to `off` in both, which is the point of it.
        let state = match (&self.log, self.runtime.status) {
            (
                Some(log),
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping,
            ) => log.state(),
            _ => policy_state(logging.source, logging.mode),
        };

        // The file this session is writing *now* when it is writing one, and
        // otherwise the file the last run left behind. The two differ for the
        // modes that decide mid-run: an `on_error` run that has been saved, and
        // a `manual` run that is recording, have a file before their record
        // does — and a UI that only knew about the record would report "not
        // recording" about a session that is writing to disk at that moment.
        let live_file = self
            .log
            .as_ref()
            .filter(|log| log.is_writing())
            .and_then(|log| log.hub_log_path())
            .map(|path| path.display().to_string());

        let log_file = live_file.or_else(|| {
            // From the run record, so a `capture` run's file stays named
            // after the run has ended and an `on_error` run that failed
            // keeps the file it just wrote.
            self.record
                .as_ref()
                .and_then(|record| record.log_file.clone())
        });

        // Whether that file is on disk, asked of the file the Logs tab's
        // actions would act on. An `external` session's current file is the
        // application's own (D-005), whatever a run record may also name — the
        // same choice the frontend's `currentLogPath` makes, so the two agree
        // on which file this answer is about.
        let current_file = match logging.source {
            LogSource::External => logging.external_path.as_deref(),
            _ => log_file.as_deref(),
        };
        let log_file_present = current_file.is_some_and(|path| Path::new(path).exists());

        LogStatus {
            session_id: session_id.to_owned(),
            mode: logging.mode,
            source: logging.source,
            state,
            truncated: self.log.as_ref().is_some_and(|log| log.is_truncated()),
            log_file,
            log_file_present,
            external_log: logging.external_path.clone(),
            session_log_dir: session_dir(
                roots,
                session_id,
                self.runtime
                    .started_at
                    .or(self.record.as_ref().map(|record| record.started_at)),
            ),
            records_input: false,
            buffer: lock(&self.buffer).summary(),
            last_error: self
                .log
                .as_ref()
                .and_then(|log| log.last_error())
                .or_else(|| self.log_problem.clone()),
        }
    }
}

/// The folder a session's Hub-written logs are in, or `None` without roots or
/// a filesystem-safe id. Computed, never created: an empty directory for a
/// session that has never run is noise (D-004).
///
/// The month is the one the session's *current* run started in, not the one it
/// is now (`docs/LOGGING.md` §5 files runs by the month they started). For a
/// session left running across a month boundary those differ, and a "open the
/// log folder" that opened the wrong month's folder would show a user an empty
/// directory next to a log that exists.
fn session_dir(
    roots: Option<&LogRoots>,
    session_id: &str,
    started_at: Option<Timestamp>,
) -> Option<String> {
    let roots = roots?;
    let at = started_at.unwrap_or_else(Timestamp::now);
    let dir = config::session_log_dir(
        &roots.logs_dir,
        session_id,
        at.unix_secs()?,
        config::local_utc_offset_secs(),
    )?;
    Some(dir.display().to_string())
}

/// Why this session has no log file, in the policy's own words.
///
/// `docs/DEVELOPMENT.md` §9 asks an error for a cause *and* something to do.
/// "No log file" has four different causes here, and the one that matters is
/// `on_error`: the user asked for the log of a run that has not failed, and the
/// answer is that they can commit the buffer right now.
fn policy_reason(status: &LogStatus) -> String {
    match (status.source, status.mode) {
        (LogSource::None, _) => {
            "`source: none` keeps output in the in-memory buffer and writes nothing to disk"
                .to_owned()
        }
        (_, EffectiveLogMode::Off) => {
            "`mode: off` keeps output in the in-memory buffer and writes nothing to disk".to_owned()
        }
        (_, EffectiveLogMode::Manual) => {
            "`mode: manual` writes nothing until recording is switched on for the run".to_owned()
        }
        (_, EffectiveLogMode::OnError) => {
            "`mode: on_error` writes a file only when a run ends badly — `save_run_log` commits \
             the buffer now"
                .to_owned()
        }
        (_, EffectiveLogMode::Always) => {
            "`mode: always` writes a file as soon as the session runs".to_owned()
        }
    }
}

/// How often a run watcher re-checks whether it has been superseded.
///
/// The watcher is blocked on the supervisor's exit condition between ticks, so
/// this costs one wake-up per interval per running session and no CPU worth
/// measuring — well inside the budget D-009 sets for background work.
const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// The port whose readiness a run should be probed on, or `None` when there is
/// nothing to probe (T08 §12).
///
/// Only a service that names a port. A service without one has no readiness
/// question this layer can answer — whether its process is alive is what
/// `Running` already says, and repeating that as a "health reading" would be
/// two renderings of one fact. A terminal has neither a port nor a `port` field
/// to put one in: the config schema refuses `port` on a terminal session
/// (`config::validate`), and what it *can* answer — whether a shell is attached
/// — is already on the snapshot as `pty_attached`.
fn health_port(config: &SessionConfig, run: &Run) -> Option<u16> {
    match (config.session_type, run) {
        (SessionType::Service, Run::Process(_)) => config.port,
        _ => None,
    }
}

/// Notice a run ending without anyone asking, and record it.
///
/// This is the mechanism behind spec §5 rule 4 — an unexpected exit updates
/// session state even while the UI is hidden. Nothing here consults the UI:
/// the watcher owns the run, the session state is the truth, and the sink is
/// where anybody interested finds out.
///
/// It only ever applies to the run it was started for. If the session was
/// restarted, or a stop already took ownership of the ending, the watcher
/// returns without publishing: a second "ended" for a run that is already
/// accounted for would be worse than the missed wake-up it costs.
fn watch_run(
    core: SessionCore,
    handle: Arc<Mutex<SessionState>>,
    session_id: String,
    generation: u64,
) {
    let (run, polled_port) = {
        let state = lock(&handle);
        if state.generation != generation {
            return;
        }
        match &state.run {
            Some(run) => (run.clone(), health_port(&state.config, run)),
            None => return,
        }
    };

    // Health rides this thread rather than one of its own: the watcher exists
    // exactly as long as the run does, which is what "cancellable with session
    // lifecycle" asks for and what keeps a stopped service from leaving a
    // monitor behind (spec §12, §14). The first probe is due immediately — a
    // service that is already listening should not wait an interval to be
    // called ready — and every one after it is an interval apart.
    let mut next_probe = polled_port.map(|_| std::time::Instant::now());

    loop {
        if run.wait_for_exit(WATCH_TICK).is_some() {
            break;
        }
        // Still alive: stop waiting if this watcher no longer owns the run.
        if lock(&handle).generation != generation {
            return;
        }
        // This tick is also the pipeline's slow flush for a run whose output
        // arrives in bursts that never fill a batch: a service's reader has no
        // tick of its own (it blocks on the pipe), so without this the last
        // lines before a quiet spell would sit unpublished. For a terminal the
        // reader ticks faster and this is usually a no-op.
        core.flush_output(&session_id, generation, false);

        if let (Some(port), Some(due)) = (polled_port, next_probe) {
            let now = std::time::Instant::now();
            if now >= due {
                core.read_health(&session_id, &handle, &run, port, generation);
                next_probe = Some(now + core.health_interval);
            }
        }
    }

    // The run's own process ending is not the run ending. A terminal's shell
    // can hand a child off and exit, and the session must not be presented as
    // ended while the tree the Hub owns for it is still running (spec #59
    // decision 13). Settled before the state is closed, so nothing of the run
    // outlives the ending that reports it.
    let settled = run.settle_tree();

    let ending = {
        let mut state = lock(&handle);
        // Superseded by a restart, or a stop/report already owns this ending.
        if state.generation != generation || state.runtime.status != SessionStatus::Running {
            return;
        }
        let code = run.exit_status().and_then(|exit| exit.code);

        if let Err(message) = settled {
            // The shell ended but its tree did not: saying `Exited` here would
            // be exactly the claim the barrier exists to prevent, so the run is
            // reported as the failure it was and the reason is carried on the
            // snapshot (§4 keeps a last structured error for this).
            state.runtime.last_error = Some(SessionErrorInfo {
                operation: "run".to_owned(),
                message,
            });
            state.close_run(SessionStatus::Error, code);
            RunEnding::Failed
        } else {
            // Spec §5 allows both `Running -> Exited` and `Running -> Error`, so
            // the two are not interchangeable. A run that ends by itself with a
            // failing status has failed — saying `Exited` would show a crashed
            // service as a cleanly stopped one, leave the error count at zero,
            // and leave §11's "Restart Failed" with nothing to notice. A run
            // with no code to inspect is not called a failure.
            let ended = match code {
                Some(0) | None => SessionStatus::Exited,
                Some(_) => SessionStatus::Error,
            };
            if ended == SessionStatus::Error {
                state.runtime.last_error = Some(SessionErrorInfo {
                    operation: "run".to_owned(),
                    message: format!(
                        "the run ended on its own with exit code {}",
                        code.unwrap_or_default()
                    ),
                });
            }
            state.close_run(ended, code);
            match ended {
                SessionStatus::Error => RunEnding::Failed,
                // `Exited` after nobody asked: a clean exit that was not
                // requested.
                _ => RunEnding::OnItsOwn,
            }
        }
    };

    // The run's log is closed and its record filed before anything is
    // published, so a listener that reacts to the ending already has the path
    // of the file it produced.
    core.finalize_run(&session_id, &handle, ending);
    core.publish_ending(&session_id);
}

/// Build the spec for a supervised service run, or explain why there is none.
///
/// Reached only for a service session ([`start_spec`] dispatches by type); a
/// terminal's run is a PTY, which T02 hosts and T07 wires to this same
/// timeline.
///
/// The run's resolved logging decides whether its output is piped back: a
/// session whose policy captures (`docs/LOGGING.md` §8 — every session keeps a
/// scrollback) gets [`OutputMode::Capture`], and one linked to an
/// application-owned log does not, because taking its output would duplicate
/// what the application already writes (D-005).
fn process_spec(
    config: &SessionConfig,
    id: &str,
    planned: &PlannedRun,
) -> Result<ProcessSpec, SessionError> {
    const OPERATION: &str = "start";

    let Some(command) = config.command.as_deref() else {
        return Err(SessionError::failed(
            id,
            OPERATION,
            format!("session `{id}` has no `command` to start"),
            None,
        ));
    };
    let Some(cwd) = config.cwd.clone() else {
        // `cwd` is optional in the schema, but starting a service in whatever
        // directory the app happens to inherit is not predictable (DEVELOPMENT
        // §16). Refuse and say what to add rather than guess.
        return Err(SessionError::failed(
            id,
            OPERATION,
            format!(
                "session `{id}` has no `cwd`; add one so the service starts in a known \
                 directory"
            ),
            None,
        ));
    };

    let (program, args) = split_command(command).map_err(|reason| {
        SessionError::failed(
            id,
            OPERATION,
            format!("session `{id}` has an unusable command: {reason}"),
            None,
        )
    })?;

    // A standalone application keeps the console it provides, so nothing of
    // its output is piped back — and its configuration cannot ask for capture
    // in the first place (validation refuses `source: captured` with
    // `display: window`, spec #59 decision 16). Stated here as well because
    // this is the function that decides it for a run.
    let output = if planned.captures_output() && !config.is_window() {
        OutputMode::Capture
    } else {
        OutputMode::Inherit
    };
    Ok(ProcessSpec {
        program,
        args,
        cwd,
        output,
    })
}

/// Split a configured command line into a program and its arguments.
///
/// `pub(crate)` since #66: the "添加应用" form's display recommendation asks
/// which program a command would start, and asking a *second* tokenizer would
/// let the advice be about a program the launch would not run.
///
/// The command is run directly rather than through `cmd.exe /C`. That keeps
/// the run's process id the service's own id — which is what the session
/// header shows and what the user will find in Task Manager — instead of a
/// short-lived shell's. Ownership and kill-tree behaviour do not depend on
/// this choice: the supervisor owns the run's job object either way.
///
/// Double quotes group a run of characters that would otherwise split, which
/// is what makes a path like `"C:\Program Files\node\node.exe"` usable. There
/// is no escape handling beyond that: a literal quote inside an argument is
/// not expressible yet, and is refused rather than silently mis-split.
pub(crate) fn split_command(command: &str) -> Result<(PathBuf, Vec<String>), String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_token = false;

    for character in command.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                // An empty quoted argument (`""`) is still an argument.
                in_token = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            c => {
                current.push(c);
                in_token = true;
            }
        }
    }

    if quoted {
        return Err(format!("unbalanced quote in `{command}`"));
    }
    if in_token {
        tokens.push(current);
    }

    let mut tokens = tokens.into_iter();
    let Some(program) = tokens.next() else {
        return Err("the command is empty".to_owned());
    };
    Ok((PathBuf::from(program), tokens.collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EffectiveLogMode, EffectiveLogging, LogSource, SessionType};
    use std::net::{Ipv4Addr, TcpListener};
    use std::path::PathBuf;

    /// A sink that keeps what it was told, so a test can assert on the event
    /// stream the same way the frontend would consume it.
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl RecordingSink {
        fn names(&self) -> Vec<&'static str> {
            lock(&self.events).iter().map(SessionEvent::name).collect()
        }

        fn events(&self) -> Vec<SessionEvent> {
            lock(&self.events).clone()
        }
    }

    impl EventSink for RecordingSink {
        fn publish(&self, event: SessionEvent) {
            lock(&self.events).push(event);
        }
    }

    /// Both listeners see the same event. The window's transport and the tray
    /// have nothing to say to each other, but neither may be skipped — that is
    /// the whole point of having both behind one core.
    #[test]
    fn a_fanout_reaches_every_sink() {
        let transport = Arc::new(RecordingSink::default());
        let tray = Arc::new(RecordingSink::default());
        let fanout = FanoutSink::new(vec![transport.clone(), tray.clone()]);

        fanout.publish(SessionEvent::AppSummaryChanged(AppSummaryChanged {
            summary: AppSummary::default(),
        }));

        assert_eq!(transport.names(), vec!["app-summary-changed"]);
        assert_eq!(tray.names(), vec!["app-summary-changed"]);
    }

    /// A Hub with no listeners is a Hub, not a panic — the same guarantee
    /// `NoopSink` gives, and what keeps a core constructible before anything
    /// has decided who is listening.
    #[test]
    fn a_fanout_with_no_sinks_discards_quietly() {
        FanoutSink::new(Vec::new()).publish(SessionEvent::AppSummaryChanged(AppSummaryChanged {
            summary: AppSummary::default(),
        }));
    }

    /// A command that stays alive until it is stopped, so a test can observe a
    /// session while it is genuinely `Running`. `ping` is the same long-running
    /// console child the T03 smoke test uses.
    const LONG_RUNNING: &str = "cmd.exe /c ping -n 120 127.0.0.1";

    fn service(id: &str) -> SessionConfig {
        SessionConfig {
            id: id.to_owned(),
            name: format!("Service {id}"),
            session_type: SessionType::Service,
            cwd: Some(test_cwd()),
            command: Some("cmd.exe /c exit 0".to_owned()),
            url: None,
            port: None,
            purpose: None,
            close_impact: None,
            shell: None,
            initial_command: None,
            display: DisplayMode::Internal,
            lifecycle: LifecycleOwner::Managed,
            logging: EffectiveLogging {
                mode: EffectiveLogMode::Always,
                source: LogSource::Captured,
                external_path: None,
            },
        }
    }

    fn test_cwd() -> PathBuf {
        std::env::current_dir().expect("the test process has a working directory")
    }

    /// A machine whose shells are exactly the ones a test names (#62).
    ///
    /// The preference order and the directory rule have their own suite in
    /// [`crate::session::temporary`]; what this is for is deciding *which*
    /// shell a kernel-level test creates a session with — and, in the failure
    /// cases, that there is none.
    struct OneShell {
        name: &'static str,
        program: PathBuf,
    }

    impl crate::session::temporary::ShellLookup for OneShell {
        fn find(&self, name: &str) -> Option<PathBuf> {
            (name == self.name).then(|| self.program.clone())
        }
    }

    /// A machine with no PowerShell installed at all.
    struct NoShells;

    impl crate::session::temporary::ShellLookup for NoShells {
        fn find(&self, _name: &str) -> Option<PathBuf> {
            None
        }
    }

    /// The path T02's and T07's suites use, so nothing here depends on how
    /// `PATH` happens to be set. Only a test that starts a shell needs it.
    fn windows_powershell() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\PowerShell.exe")
    }

    fn core_with(id: &str) -> (SessionCore, Arc<RecordingSink>) {
        core_with_command(id, "cmd.exe /c exit 0")
    }

    fn core_with_command(id: &str, command: &str) -> (SessionCore, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink.clone());
        let mut config = service(id);
        config.command = Some(command.to_owned());
        core.register(config).expect("registration succeeds");
        (core, sink)
    }

    /// The plan a run of `config` would get, resolved against no writable
    /// root — the shape every unit test here works with.
    fn planned_for(config: &SessionConfig) -> PlannedRun {
        PlannedRun {
            run_id: RunId::mint(),
            started_at: Timestamp::now(),
            plan: LogPlan::resolve(&config.logging, None),
        }
    }

    #[test]
    fn starting_a_service_reports_running_with_a_pid_and_run_id() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);

        let runtime = core.start("svc").expect("start succeeds");
        assert_eq!(runtime.status, SessionStatus::Running);
        assert!(runtime.pid.is_some(), "a running session has a pid");
        assert!(runtime.run_id.is_some(), "a run has an id");
        assert!(runtime.started_at.is_some(), "a run has a start time");
        assert_eq!(runtime.exit_code, None, "it has not ended");
        assert_eq!(runtime.last_error, None);

        let summary = core.summary();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.running, 1);
        assert_eq!(summary.error, 0);

        core.force_stop("svc").expect("cleanup");
    }

    #[test]
    fn a_member_without_a_creation_time_leaves_the_tree_unread() {
        assert!(super::record_known([Some((1, 10)), None, Some((3, 30))]).is_none());
        let complete = super::record_known([Some((1, 10)), Some((2, 20))]).expect("both recorded");
        assert_eq!(
            complete,
            vec![
                ProcessIdentity::for_test(1, 10),
                ProcessIdentity::for_test(2, 20),
            ]
        );
        assert_eq!(
            super::record_known(std::iter::empty::<Option<(u32, u64)>>()),
            Some(vec![])
        );
    }

    /// The port page reads sessions that still hold a live process (#98).
    ///
    /// A registered session that has not been started is not a process. A
    /// running one is its pid plus the creation time observed now, and every
    /// tree member is recorded the same way. Stopping it drops the snapshot.
    /// The sleeper is the same `ping` the other lifecycle tests use, and it
    /// is force-stopped on the way out — including when an assertion fails.
    #[test]
    fn listening_processes_follow_a_live_run_only() {
        let idle = SessionCore::without_listener();
        assert!(
            idle.listening_processes().is_empty(),
            "a core with no sessions has nothing to attribute"
        );

        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        assert!(
            core.listening_processes().is_empty(),
            "a registered session that is not running is not a listener owner"
        );

        let runtime = core.start("svc").expect("start succeeds");
        let pid = runtime.pid.expect("a running session has a pid");

        struct StopOnDrop<'a>(&'a SessionCore);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                let _ = self.0.force_stop("svc");
            }
        }
        let guard = StopOnDrop(&core);

        let listed = core.listening_processes();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "svc");
        assert_eq!(listed[0].identity.pid(), pid);
        assert!(
            listed[0].identity.matches(),
            "the snapshot's identity is the process the run is holding"
        );
        let tree = listed[0]
            .tree
            .as_ref()
            .expect("the managed job can be listed; a failure would be None");
        assert!(
            tree.iter().any(|member| member.pid() == pid),
            "the job lists the run's own process"
        );
        for member in tree {
            let Some(created) = crate::process::creation_time_of(member.pid()) else {
                continue;
            };
            assert_eq!(
                member.created_at(),
                created,
                "a member stores the creation time captured at listing"
            );
        }

        drop(guard);
        assert!(
            core.listening_processes().is_empty(),
            "a stopped session is not still offered as an owner"
        );
    }

    /// A stop the ladder could not confirm leaves the session in `Error` while
    /// it still owns the run (#125). That run is what the port page has to
    /// attribute, and what `confirm_end_occupant` has to protect from being
    /// ended outside the session (#129).
    #[test]
    fn an_error_session_that_still_owns_its_run_is_still_listed() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        let runtime = core.start("svc").expect("start succeeds");
        let pid = runtime.pid.expect("a running session has a pid");
        {
            let handle = core.handle("svc").expect("registered");
            lock(&handle).runtime.status = SessionStatus::Error;
        }

        let listed = core.listening_processes();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity.pid(), pid);

        core.force_stop("svc").expect("cleanup");
    }

    /// The listener has to see `Starting` before `Running`: a UI that only
    /// ever heard `Running` could not show that a start was in flight.
    #[test]
    fn starting_publishes_the_states_it_passed_through() {
        let (core, sink) = core_with_command("svc", LONG_RUNNING);

        core.start("svc").expect("start succeeds");

        let statuses: Vec<SessionStatus> = sink
            .events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::StateChanged(inner) => Some(inner.runtime.status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![SessionStatus::Starting, SessionStatus::Running]
        );
        assert_eq!(
            sink.names(),
            vec![
                "session-state-changed",
                "app-summary-changed",
                "session-state-changed",
                "app-summary-changed",
                "run-record-updated",
            ],
            "start should announce the run it created"
        );

        core.force_stop("svc").expect("cleanup");
    }

    #[test]
    fn the_run_record_carries_the_run_identity() {
        let (core, sink) = core_with_command("svc", LONG_RUNNING);

        let runtime = core.start("svc").expect("start succeeds");

        let record = sink
            .events()
            .into_iter()
            .find_map(|event| match event {
                SessionEvent::RunRecordUpdated(inner) => Some(inner.run),
                _ => None,
            })
            .expect("a run record was published");

        assert_eq!(record.session_id, "svc");
        assert_eq!(record.pid, runtime.pid);
        assert_eq!(Some(record.run_id), runtime.run_id);
        assert_eq!(record.exit_code, None, "the run is still open");
        assert_eq!(record.ended_at, None, "the run is still open");
        assert_eq!(record.log_source, LogSource::Captured);

        core.force_stop("svc").expect("cleanup");
    }

    /// Whether the OS still lists a process under this id. Used to check that
    /// a replaced run is really gone rather than merely forgotten by Session
    /// Core, which is the half of "restart cannot leave duplicates" that the
    /// in-process state cannot answer on its own.
    fn pid_is_alive(pid: u32) -> bool {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }

    /// Spec §5 rule 2 and T04's acceptance criterion 4.
    #[test]
    fn restarting_replaces_the_run_and_leaves_exactly_one_alive() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        let first = core.start("svc").expect("start succeeds");
        let first_pid = first.pid.expect("a running session has a pid");

        let second = core.restart("svc").expect("restart succeeds");

        assert_eq!(second.status, SessionStatus::Running);
        assert_ne!(second.run_id, first.run_id, "a restart is a new run");
        assert_ne!(second.pid, first.pid, "a restart is a new process");
        assert_eq!(core.summary().running, 1, "one session, one run");
        assert!(
            !pid_is_alive(first_pid),
            "the replaced run (pid {first_pid}) outlived the restart"
        );

        core.force_stop("svc").expect("cleanup");
    }

    /// A restart of something that is not running is just a start — that is
    /// what makes the button usable on a `Stopped` or failed session.
    #[test]
    fn restarting_a_session_that_is_not_running_starts_it() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);

        let runtime = core.restart("svc").expect("restart starts it");
        assert_eq!(runtime.status, SessionStatus::Running);
        assert!(runtime.pid.is_some());

        core.force_stop("svc").expect("cleanup");
    }

    #[test]
    fn restarting_an_unknown_session_is_refused() {
        let (core, _sink) = core_with("svc");

        let error = core.restart("nope").expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::UnknownSession);
    }

    /// A process this test started, listening on a port no session owns (#106).
    ///
    /// Restart must not end it. The guard kills it on the way out, including
    /// when an assertion fails, so a failed run does not leave the listener up.
    struct ExternalListener {
        child: std::process::Child,
        port: u16,
        pid: u32,
    }

    impl ExternalListener {
        fn start() -> Self {
            let port = reserve_local_port();
            let child = std::process::Command::new(windows_powershell())
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &listen_script(port),
                ])
                .spawn()
                .expect("the external listener starts");
            let pid = child.id();
            let listener = Self { child, port, pid };
            wait_for_tcp_pid("the external listener to bind", port, pid);
            listener
        }
    }

    impl Drop for ExternalListener {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    struct StopSessions<'a> {
        core: &'a SessionCore,
        ids: &'a [&'a str],
    }

    impl Drop for StopSessions<'_> {
        fn drop(&mut self) {
            for id in self.ids {
                let Some(runtime) = self.core.snapshot(id) else {
                    continue;
                };
                if matches!(
                    runtime.status,
                    SessionStatus::Running | SessionStatus::Starting | SessionStatus::Stopping
                ) {
                    let _ = self.core.force_stop(id);
                }
            }
        }
    }

    fn reserve_local_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind an ephemeral port");
        listener.local_addr().expect("the bound address").port()
    }

    fn listen_script(port: u16) -> String {
        format!(
            "$l = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, {port}); $l.Start(); Start-Sleep -Seconds 180"
        )
    }

    /// The managed command whose own process listens on `port`.
    fn listening_service_command(port: u16) -> String {
        format!(
            "\"{}\" -NoProfile -NonInteractive -Command \"{}\"",
            windows_powershell().display(),
            listen_script(port)
        )
    }

    fn register_service(core: &SessionCore, id: &str, command: &str, port: Option<u16>) {
        let mut config = service(id);
        config.command = Some(command.to_owned());
        config.port = port;
        core.register(config).expect("registration succeeds");
    }

    fn tcp_listener_pids(port: u16) -> Vec<u32> {
        crate::listen::collect()
            .expect("the listener table can be read")
            .into_iter()
            .filter(|row| row.protocol == crate::listen::Protocol::Tcp && row.port == port)
            .filter_map(|row| row.pid.as_ref().copied())
            .collect()
    }

    fn wait_for_tcp_pid(what: &str, port: u16, pid: u32) {
        let timeout = std::time::Duration::from_secs(20);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let pids = tcp_listener_pids(port);
            if pids.contains(&pid) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: port {port} was not owned by pid {pid} within {timeout:?}; last pids {pids:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn port_still_accepts(port: u16) -> bool {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            std::time::Duration::from_millis(500),
        )
        .is_ok()
    }

    fn assert_still_this_run(core: &SessionCore, id: &str, pid: u32) {
        let runtime = core.snapshot(id).expect("the session is registered");
        assert_eq!(runtime.status, SessionStatus::Running, "{id} was stopped");
        assert_eq!(runtime.pid, Some(pid), "{id} was replaced");
        assert!(!runtime.external, "{id} was turned into an external run");
        assert!(pid_is_alive(pid), "{id} pid {pid} is gone");
    }

    /// Latest published record for each run, in the order the runs appeared.
    fn latest_run_records(sink: &RecordingSink, session_id: &str) -> Vec<RunRecord> {
        let mut records: Vec<RunRecord> = Vec::new();
        for event in sink.events() {
            let SessionEvent::RunRecordUpdated(update) = event else {
                continue;
            };
            if update.session_id != session_id {
                continue;
            }
            if let Some(slot) = records
                .iter_mut()
                .find(|record| record.run_id == update.run.run_id)
            {
                *slot = update.run;
            } else {
                records.push(update.run);
            }
        }
        records
    }

    /// Port monitoring must not change what a restart is (#106): end this run,
    /// then start the next one. The session's own listener is not an external
    /// occupier, an external holder is not ended, and no other session moves.
    #[test]
    fn restarting_under_port_monitoring_replaces_one_run_and_leaves_other_holders() {
        let external = ExternalListener::start();
        let own_port = reserve_local_port();

        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink.clone())
            .with_health_interval(std::time::Duration::from_millis(200));
        let ids = ["plain", "neighbour", "own", "blocked", "held"];
        register_service(&core, "plain", LONG_RUNNING, None);
        register_service(&core, "neighbour", LONG_RUNNING, None);
        register_service(
            &core,
            "own",
            &listening_service_command(own_port),
            Some(own_port),
        );
        register_service(&core, "blocked", LONG_RUNNING, Some(external.port));
        register_service(&core, "held", LONG_RUNNING, Some(external.port));
        let _stop = StopSessions {
            core: &core,
            ids: &ids,
        };

        let plain_first = core.start("plain").expect("the no-port session starts");
        let plain_first_pid = plain_first.pid.expect("a running session has a pid");
        let neighbour = core.start("neighbour").expect("the neighbour starts");
        let neighbour_pid = neighbour.pid.expect("a running session has a pid");
        let own_first = core.start("own").expect("the listening session starts");
        let own_first_pid = own_first.pid.expect("a running session has a pid");
        let own_first_run = own_first.run_id.clone().expect("the run has an id");
        let blocked_first = core.start("blocked").expect("the blocked session starts");
        let blocked_first_pid = blocked_first.pid.expect("a running session has a pid");
        wait_for_tcp_pid(
            "the session's own process to listen on its configured port",
            own_port,
            own_first_pid,
        );

        // No configured port: the old process is gone, a new run is up, and
        // the neighbour is the same process it was.
        let plain_second = core.restart("plain").expect("a no-port session restarts");
        let plain_pid = plain_second.pid.expect("the new run has a pid");
        assert_eq!(plain_second.status, SessionStatus::Running);
        assert_ne!(plain_second.run_id, plain_first.run_id);
        assert_ne!(plain_pid, plain_first_pid);
        assert!(
            !pid_is_alive(plain_first_pid),
            "the replaced no-port run (pid {plain_first_pid}) outlived the restart"
        );
        assert_still_this_run(&core, "neighbour", neighbour_pid);
        assert_still_this_run(&core, "own", own_first_pid);
        assert_still_this_run(&core, "blocked", blocked_first_pid);
        assert!(
            pid_is_alive(external.pid) && port_still_accepts(external.port),
            "restarting a no-port session ended the external holder"
        );

        // The listener is this session's own process. Restart stops that run
        // and starts the next one; it does not stop on an occupancy refusal,
        // and it does not leave the previous run's record open.
        let own_second = core
            .restart("own")
            .expect("a session listening on its own port restarts");
        let own_pid = own_second.pid.expect("the replacement run has a pid");
        assert_eq!(own_second.status, SessionStatus::Running);
        assert!(!own_second.external);
        assert_ne!(own_second.run_id.as_ref(), Some(&own_first_run));
        assert_ne!(own_pid, own_first_pid);
        assert!(
            !pid_is_alive(own_first_pid),
            "the replaced listener (pid {own_first_pid}) outlived the restart"
        );
        wait_for_tcp_pid(
            "the replacement run to listen on the configured port",
            own_port,
            own_pid,
        );
        assert!(
            !tcp_listener_pids(own_port).contains(&own_first_pid),
            "the old listener is still treated as holding the port"
        );
        let own_records = latest_run_records(&sink, "own");
        let own_open: Vec<_> = own_records
            .iter()
            .filter(|record| record.ended_at.is_none())
            .collect();
        assert_eq!(
            own_open.len(),
            1,
            "restart left an extra open run record: {own_records:?}"
        );
        assert_eq!(own_open[0].pid, Some(own_pid));
        assert_eq!(own_second.run_id.as_ref(), Some(&own_open[0].run_id));
        assert!(
            own_records
                .iter()
                .any(|record| record.run_id == own_first_run && record.ended_at.is_some()),
            "the replaced run's record was not closed: {own_records:?}"
        );
        let reading = wait_for_health(
            &core,
            "own",
            |reading| reading.port_open && reading.process_alive,
            std::time::Duration::from_secs(10),
        );
        assert!(reading.process_alive && reading.port_open);
        let logs = core
            .log_status("own")
            .expect("logs still answer after restart");
        assert_eq!(logs.session_id, "own");
        assert_eq!(logs.source, LogSource::Captured);
        let details = core.snapshot("own").expect("details still answer");
        assert_eq!(details.status, SessionStatus::Running);
        assert_eq!(details.pid, Some(own_pid));
        assert_eq!(details.run_id, own_second.run_id);
        assert_eq!(core.summary().running, 4);
        assert_still_this_run(&core, "plain", plain_pid);
        assert_still_this_run(&core, "neighbour", neighbour_pid);
        assert_still_this_run(&core, "blocked", blocked_first_pid);
        assert!(
            pid_is_alive(external.pid) && port_still_accepts(external.port),
            "restarting the listening session ended the external holder"
        );
        let listed = core.listening_processes();
        assert!(listed
            .iter()
            .any(|session| session.id == "own" && session.identity.pid() == own_pid));
        assert!(!listed
            .iter()
            .any(|session| session.identity.pid() == external.pid));
        assert!(!listed
            .iter()
            .any(|session| session.identity.pid() == own_first_pid));

        // An external program holds the configured port. Restart replaces this
        // session's run and does not end that program or the neighbour.
        let blocked_second = core
            .restart("blocked")
            .expect("restart does not refuse because an external program holds the port");
        let blocked_pid = blocked_second.pid.expect("the replacement run has a pid");
        assert_eq!(blocked_second.status, SessionStatus::Running);
        assert_ne!(blocked_pid, external.pid);
        assert_ne!(blocked_pid, blocked_first_pid);
        assert!(!pid_is_alive(blocked_first_pid));
        assert!(
            pid_is_alive(external.pid) && port_still_accepts(external.port),
            "restart ended the external program that holds the configured port"
        );
        assert!(
            tcp_listener_pids(external.port).contains(&external.pid),
            "the external holder no longer owns its port"
        );
        assert!(!tcp_listener_pids(external.port).contains(&blocked_pid));
        assert_still_this_run(&core, "plain", plain_pid);
        assert_still_this_run(&core, "neighbour", neighbour_pid);
        assert_still_this_run(&core, "own", own_pid);

        // The same holder, for a session that was not running. Restart starts
        // it. The holder stays, and the open record is this run's process.
        let held = core
            .restart("held")
            .expect("restarting a stopped session does not refuse on the external holder");
        let held_pid = held.pid.expect("the new run has a pid");
        assert_eq!(held.status, SessionStatus::Running);
        assert_ne!(held_pid, external.pid);
        assert!(pid_is_alive(held_pid));
        let held_open: Vec<_> = latest_run_records(&sink, "held")
            .into_iter()
            .filter(|record| record.ended_at.is_none())
            .collect();
        assert_eq!(
            held_open.len(),
            1,
            "restart of a stopped session should publish exactly one open run: {held_open:?}"
        );
        assert_eq!(held_open[0].pid, Some(held_pid));
        assert_ne!(held_open[0].pid, Some(external.pid));
        assert!(
            pid_is_alive(external.pid) && port_still_accepts(external.port),
            "starting through restart ended the external holder"
        );
        assert_still_this_run(&core, "plain", plain_pid);
        assert_still_this_run(&core, "neighbour", neighbour_pid);
        assert_still_this_run(&core, "own", own_pid);
        assert_still_this_run(&core, "blocked", blocked_pid);
        let held_details = core.snapshot("held").expect("details still answer");
        assert_eq!(held_details.status, SessionStatus::Running);
        assert_eq!(held_details.pid, Some(held_pid));
        assert_eq!(core.summary().running, 5);
    }

    /// A long-running service that names `port`, with health polls at
    /// `interval` so a test does not wait out the product's own cadence.
    fn core_with_port(
        id: &str,
        port: u16,
        interval: std::time::Duration,
    ) -> (SessionCore, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink.clone()).with_health_interval(interval);
        let mut config = service(id);
        config.command = Some(LONG_RUNNING.to_owned());
        config.url = Some(format!("http://127.0.0.1:{port}"));
        config.port = Some(port);
        core.register(config).expect("registration succeeds");
        (core, sink)
    }

    /// T08 §12, first half: a running service that names a port gets a reading.
    ///
    /// The listener here is the *test's*, not the service's — `cmd.exe /c ping`
    /// never listens on anything. That is what makes this an assertion about the
    /// probe rather than about the state machine: the reading follows the
    /// socket, not the session's own status.
    #[test]
    fn a_running_service_reads_its_configured_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind an ephemeral port");
        let port = listener.local_addr().expect("the bound address").port();
        let (core, _sink) = core_with_port("svc", port, health::POLL_INTERVAL);
        core.start("svc").expect("start succeeds");

        let reading = wait_for_health(
            &core,
            "svc",
            |reading| reading.port_open,
            std::time::Duration::from_secs(10),
        );
        assert!(reading.process_alive, "the run's process is alive");

        drop(listener);
        core.force_stop("svc").expect("cleanup");
    }

    /// T08's acceptance criterion that port availability is not lifecycle
    /// state: a service whose port is not answering is still `Running`, and the
    /// reading says both facts rather than picking one (D-008).
    #[test]
    fn a_running_service_whose_port_is_closed_reports_both_facts() {
        let (core, _sink) = core_with_port("svc", health::closed_port(), health::POLL_INTERVAL);
        core.start("svc").expect("start succeeds");

        let reading = wait_for_health(
            &core,
            "svc",
            |reading| !reading.port_open,
            std::time::Duration::from_secs(10),
        );

        assert!(
            reading.process_alive,
            "nothing listening is not the same as nothing running"
        );
        assert_eq!(
            core.snapshot("svc").expect("registered").status,
            SessionStatus::Running,
            "a closed port must not move the lifecycle"
        );

        core.force_stop("svc").expect("cleanup");
    }

    /// A service that names no port has no readiness question this layer can
    /// answer, so it is never probed: no reading is not "the port is closed".
    #[test]
    fn a_service_with_no_port_is_never_probed() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");

        // Several watcher ticks: a probe would have landed by now.
        std::thread::sleep(std::time::Duration::from_millis(400));

        assert_eq!(
            core.snapshot("svc").expect("registered").health,
            None,
            "a session with no port to check reports no reading"
        );

        core.force_stop("svc").expect("cleanup");
    }

    /// A reading describes a run in flight, and the polling stops with it
    /// (spec §12: "cancellable with session lifecycle").
    ///
    /// The port stays open across the stop — the test's listener, not the
    /// service's — so a poller that outlived its run would keep finding it
    /// listening and put a reading back.
    #[test]
    fn a_reading_and_its_polling_go_away_with_the_run_they_described() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind an ephemeral port");
        let port = listener.local_addr().expect("the bound address").port();
        let (core, _sink) = core_with_port("svc", port, std::time::Duration::from_millis(40));
        core.start("svc").expect("start succeeds");
        wait_for_health(
            &core,
            "svc",
            |reading| reading.port_open,
            std::time::Duration::from_secs(10),
        );

        let stopped = core.force_stop("svc").expect("stop succeeds");
        assert_eq!(stopped.health, None, "a stopped session reports no reading");

        // Several poll intervals' worth of time with the port still listening.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            core.snapshot("svc").expect("registered").health,
            None,
            "something is still probing a run that has ended"
        );
        drop(listener);
    }

    /// §12 wants checks low-frequency and §14 wants no needless UI work, so a
    /// reading that has not moved is re-read without being republished.
    #[test]
    fn a_reading_is_published_only_when_it_changes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind an ephemeral port");
        let port = listener.local_addr().expect("the bound address").port();
        let (core, sink) = core_with_port("svc", port, std::time::Duration::from_millis(40));
        core.start("svc").expect("start succeeds");
        wait_for_health(
            &core,
            "svc",
            |reading| reading.port_open,
            std::time::Duration::from_secs(10),
        );

        // Many probe intervals with nothing changing.
        let settled = state_changes(&sink, "svc");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            state_changes(&sink, "svc"),
            settled,
            "an unchanged reading was republished"
        );

        // And the change itself is announced, so a window follows the service
        // coming up without polling for it.
        drop(listener);
        wait_for_health(
            &core,
            "svc",
            |reading| !reading.port_open,
            std::time::Duration::from_secs(10),
        );
        assert!(
            state_changes(&sink, "svc") > settled,
            "the reading changed without an event"
        );

        core.force_stop("svc").expect("cleanup");
    }

    /// T08 §9: "open this session's URL" resolves the URL from the session, so
    /// the caller can only name a session — never an address.
    #[test]
    fn a_session_url_comes_from_the_session_that_owns_it() {
        let mut config = service("svc");
        config.url = Some("http://127.0.0.1:8188".to_owned());
        let core = SessionCore::new(Arc::new(RecordingSink::default()));
        core.register(config).expect("registration succeeds");

        assert_eq!(
            core.session_url("svc").expect("the session has a url"),
            "http://127.0.0.1:8188"
        );
    }

    /// A session with nothing to open says what to add instead of opening
    /// nothing, and an unknown session is refused as the unknown session it is.
    #[test]
    fn a_missing_url_or_cwd_is_refused_with_what_to_add() {
        let mut config = service("svc");
        config.cwd = None;
        let core = SessionCore::new(Arc::new(RecordingSink::default()));
        core.register(config).expect("registration succeeds");

        let url = core.session_url("svc").expect_err("no url is configured");
        assert_eq!(url.kind, SessionErrorKind::Unsupported);
        assert_eq!(url.operation, "open_session_url");
        assert!(url.message.contains("url"), "message: {}", url.message);

        let cwd = core.session_cwd("svc").expect_err("no cwd is configured");
        assert_eq!(cwd.kind, SessionErrorKind::Unsupported);
        assert!(cwd.message.contains("cwd"), "message: {}", cwd.message);

        let unknown = core.session_url("nope").expect_err("no such session");
        assert_eq!(unknown.kind, SessionErrorKind::UnknownSession);
    }

    /// The working directory is resolved the same way, and comes back as the
    /// path the session was configured with.
    #[test]
    fn a_session_cwd_comes_from_the_session_that_owns_it() {
        let (core, _sink) = core_with("svc");

        assert_eq!(
            core.session_cwd("svc").expect("the session has a cwd"),
            test_cwd()
        );
    }

    /// T04 acceptance criterion 5: session state does not depend on a window
    /// being visible.
    ///
    /// The layer has no window to consult — that is the point — so the test
    /// that pins it is a core with *nobody* listening, which is exactly what a
    /// hidden window is from Session Core's side. Lifecycle proceeds and the
    /// state converges regardless.
    #[test]
    fn lifecycle_runs_with_no_listener_at_all() {
        let core = SessionCore::without_listener();
        core.register(service("svc"))
            .expect("registration succeeds");

        let started = core.start("svc").expect("start succeeds");
        assert_eq!(started.status, SessionStatus::Running);

        let stopped = core.force_stop("svc").expect("force stop succeeds");
        assert_eq!(stopped.status, SessionStatus::Exited);
        assert_eq!(core.summary().running, 0);
    }

    /// And the same for the ending nobody asked for: with no listener, an
    /// unexpected exit still has to land in the state.
    #[test]
    fn an_unexpected_exit_lands_with_no_listener_at_all() {
        let core = SessionCore::without_listener();
        let mut config = service("svc");
        config.command = Some("cmd.exe /c ping -n 2 127.0.0.1".to_owned());
        core.register(config).expect("registration succeeds");
        core.start("svc").expect("start succeeds");

        wait_for_status(
            &core,
            "svc",
            SessionStatus::Exited,
            std::time::Duration::from_secs(30),
        );
        assert_eq!(core.summary().running, 0);
    }

    /// Spec §5 rule 1: a start while already running is refused.
    #[test]
    fn starting_a_session_that_is_already_running_is_refused() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("first start succeeds");

        let error = core.start("svc").expect_err("second start is refused");
        assert_eq!(error.kind, SessionErrorKind::InvalidTransition);
        assert_eq!(error.from, Some(SessionStatus::Running));
        assert!(
            error.message.contains("running"),
            "message names the state: {error}"
        );

        core.force_stop("svc").expect("cleanup");
    }

    #[test]
    fn starting_an_unknown_session_is_refused() {
        let (core, _sink) = core_with("svc");

        let error = core.start("nope").expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::UnknownSession);
        assert!(error.message.contains("nope"), "message: {error}");
    }

    /// A program that cannot be launched leaves the session in `Error` with the
    /// reason — not silently back in `Stopped`, which would hide the failure.
    #[test]
    fn a_start_that_cannot_spawn_ends_in_error_with_the_reason() {
        let (core, sink) = core_with_command("svc", "definitely-not-a-real-program-lch --serve");

        let error = core.start("svc").expect_err("the spawn fails");
        assert_eq!(error.kind, SessionErrorKind::Failed);

        let runtime = core.snapshot("svc").expect("registered");
        assert_eq!(runtime.status, SessionStatus::Error);
        assert_eq!(runtime.pid, None);
        let recorded = runtime.last_error.expect("the reason is recorded");
        assert_eq!(recorded.operation, "start");
        assert!(
            recorded
                .message
                .contains("definitely-not-a-real-program-lch"),
            "message names the program: {}",
            recorded.message
        );

        assert!(
            sink.names().contains(&"session-state-changed"),
            "the failure is announced: {:?}",
            sink.names()
        );
        assert_eq!(core.summary().error, 1);
    }

    /// A failed start has no start time, so the previous run's must not linger
    /// on the snapshot (#127).
    #[test]
    fn a_start_that_fails_does_not_keep_the_previous_runs_start_time() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        let first = core.start("svc").expect("start succeeds");
        assert!(first.started_at.is_some());
        core.force_stop("svc").expect("stop succeeds");
        {
            let handle = core.handle("svc").expect("registered");
            lock(&handle).config.command =
                Some("definitely-not-a-real-program-lch --serve".to_owned());
        }

        let error = core.start("svc").expect_err("the spawn fails");
        assert_eq!(error.kind, SessionErrorKind::Failed);

        let runtime = core.snapshot("svc").expect("registered");
        assert_eq!(runtime.status, SessionStatus::Error);
        assert_eq!(
            runtime.started_at, None,
            "a start that never got a run has no start time"
        );
    }

    /// `Error -> Stopping` is the recovery path (#125): a session whose stop
    /// could not confirm the tree is gone still owns the run, and the stop
    /// ladder must be available to try again.
    #[test]
    fn an_error_run_can_still_be_stopped() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        let running = core.start("svc").expect("start succeeds");
        let pid = running.pid.expect("a running session has a pid");
        {
            let handle = core.handle("svc").expect("registered");
            lock(&handle).runtime.status = SessionStatus::Error;
        }

        let stopped = core.stop("svc").expect("the recovery stop succeeds");
        assert!(
            matches!(
                stopped.status,
                SessionStatus::Stopped | SessionStatus::Exited
            ),
            "the run ended: {}",
            stopped.status.as_str()
        );
        assert_eq!(stopped.pid, None);
        assert!(!pid_is_alive(pid));
    }

    /// The restart from that same `Error` ends the owned run before it starts
    /// the replacement. Starting first would drop the old handle — and with it
    /// terminate its tree — inside the start's lock.
    #[test]
    fn restart_from_an_error_ends_the_owned_run_first() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        let first = core.start("svc").expect("start succeeds");
        let first_pid = first.pid.expect("a running session has a pid");
        {
            let handle = core.handle("svc").expect("registered");
            lock(&handle).runtime.status = SessionStatus::Error;
        }

        let second = core.restart("svc").expect("restart recovers the session");
        assert_eq!(second.status, SessionStatus::Running);
        let second_pid = second.pid.expect("the replacement has a pid");
        assert_ne!(second_pid, first_pid);
        assert!(!pid_is_alive(first_pid), "the previous run was ended");

        core.force_stop("svc").expect("cleanup");
    }

    /// A start must not replace a run the session still owns: the recovery is
    /// an explicit stop, or the restart that performs one.
    #[test]
    fn starting_an_error_session_that_still_owns_its_run_is_refused() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");
        {
            let handle = core.handle("svc").expect("registered");
            lock(&handle).runtime.status = SessionStatus::Error;
        }

        let error = core.start("svc").expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::Failed);
        assert!(error.message.contains("stop"), "message: {error}");

        core.force_stop("svc").expect("cleanup");
    }

    /// Poll until the session reaches `want`, so a test asserts on the state
    /// the watcher produced rather than on how long it took to produce it.
    /// Wait until a session's health reading satisfies `want`.
    ///
    /// Health is read by the run's watcher, a tick after the run starts, so
    /// there is nothing to assert synchronously — the same reason lifecycle
    /// tests wait rather than read once.
    fn wait_for_health(
        core: &SessionCore,
        session_id: &str,
        want: impl Fn(health::ServiceHealth) -> bool,
        timeout: std::time::Duration,
    ) -> health::ServiceHealth {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let reading = core
                .snapshot(session_id)
                .expect("session is registered")
                .health;
            if let Some(reading) = reading {
                if want(reading) {
                    return reading;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session `{session_id}` reported no matching health reading within {timeout:?}; \
                 last was {reading:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The number of `session-state-changed` events published for a session —
    /// how a test tells "the reading moved" from "the reading was re-read".
    fn state_changes(sink: &Arc<RecordingSink>, session_id: &str) -> usize {
        sink.events()
            .into_iter()
            .filter(|event| match event {
                SessionEvent::StateChanged(inner) => inner.session_id == session_id,
                _ => false,
            })
            .count()
    }

    fn wait_for_status(
        core: &SessionCore,
        session_id: &str,
        want: SessionStatus,
        timeout: std::time::Duration,
    ) -> SessionRuntime {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let runtime = core.snapshot(session_id).expect("session is registered");
            if runtime.status == want {
                return runtime;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session `{session_id}` never reached {} within {timeout:?}; stuck at {}",
                want.as_str(),
                runtime.status.as_str()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Put a session into `status` without walking the lifecycle.
    ///
    /// `Starting` and `Stopping` are states a test cannot reliably catch from
    /// the outside — each is exactly as long-lived as the spawn or the stop it
    /// describes. What is under test here is the *decision* activation makes
    /// about each state, so the state is set directly; the transitions that
    /// really produce them have their own tests.
    fn force_status(core: &SessionCore, session_id: &str, status: SessionStatus) {
        let handle = core.handle(session_id).expect("the session is registered");
        lock(&handle).runtime.status = status;
    }

    /// Opening a session that is not running is a start — and opening it again
    /// while that start lives reuses it, with no second process (#64, spec #59
    /// decision 10, stories 33–34).
    #[test]
    fn activating_starts_a_stopped_session_and_reuses_the_run_afterwards() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);

        let opened = core.activate("svc").expect("a stopped session opens");
        assert!(
            opened.started,
            "nothing was running, so this created the run"
        );
        assert_eq!(opened.runtime.status, SessionStatus::Running);
        let pid = opened.runtime.pid;
        assert!(pid.is_some(), "a started run has a process");

        let again = core.activate("svc").expect("an open session opens again");
        assert!(
            !again.started,
            "the second open found the run already there"
        );
        assert_eq!(again.runtime.pid, pid, "the same process, not a second one");
        assert_eq!(again.runtime.run_id, opened.runtime.run_id);

        core.stop("svc").expect("cleanup");
    }

    /// Two opens that arrive together must not fail, and must not make two
    /// runs (#64, spec #59 decision 10, stories 33–34).
    ///
    /// The window's control and a launch request handed to the Hub are on
    /// different threads, so "at the same time" is the case the decision is
    /// actually about — and the one a lock held across the whole decision would
    /// pass by accident rather than by design.
    #[test]
    fn two_opens_that_arrive_together_create_one_run() {
        use std::sync::mpsc;

        let (core, _sink) = core_with_command("svc", LONG_RUNNING);

        let (sender, receiver) = mpsc::channel();
        let openers: Vec<_> = (0..8)
            .map(|_| {
                let core = core.clone();
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let _ = sender.send(core.activate("svc").map(|opened| opened.started));
                })
            })
            .collect();
        drop(sender);
        let outcomes: Vec<Result<bool, SessionError>> = receiver.iter().collect();
        for opener in openers {
            opener.join().expect("no opener panicked");
        }

        let failures: Vec<&SessionError> =
            outcomes.iter().filter_map(|o| o.as_ref().err()).collect();
        assert!(
            failures.is_empty(),
            "every open has to answer: {failures:?}"
        );
        assert_eq!(
            outcomes.iter().filter(|o| matches!(**o, Ok(true))).count(),
            1,
            "exactly one open creates the run: {outcomes:?}"
        );
        assert_eq!(
            core.snapshots().len(),
            1,
            "and there is one session, not two"
        );

        core.stop("svc").expect("cleanup");
    }

    /// A start already in flight is the answer: opening again must not add a
    /// second run beside the one being created.
    #[test]
    fn activating_a_starting_session_answers_with_the_start_in_flight() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        force_status(&core, "svc", SessionStatus::Starting);

        let opened = core.activate("svc").expect("a starting session answers");

        assert!(!opened.started, "the run being created is the one to keep");
        assert_eq!(opened.runtime.status, SessionStatus::Starting);
        assert!(
            core.snapshot("svc").expect("registered").run_id.is_none(),
            "no second run may be created while the first is starting"
        );
    }

    /// A stop in progress is the one state an open must not queue behind: the
    /// barrier has not finished, so starting now would be the parallel run it
    /// exists to prevent.
    #[test]
    fn activating_a_stopping_session_is_refused_rather_than_queued() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        force_status(&core, "svc", SessionStatus::Stopping);

        let error = core
            .activate("svc")
            .expect_err("a stopping session cannot be opened");

        assert_eq!(error.kind, SessionErrorKind::InvalidTransition);
        assert_eq!(error.from, Some(SessionStatus::Stopping));
        assert!(error.message.contains("stopping"), "{}", error.message);
        assert!(
            core.snapshot("svc").expect("registered").run_id.is_none(),
            "a refused open must not have started anything"
        );
    }

    /// A session that ended — on its own, or by being stopped — is openable
    /// again: opening is not a restart, it is "there is nothing running, so
    /// make one".
    #[test]
    fn activating_an_ended_session_starts_a_fresh_run() {
        let (core, _sink) = core_with_command("svc", "cmd.exe /c ping -n 2 127.0.0.1");
        core.start("svc").expect("the first start succeeds");
        wait_for_status(
            &core,
            "svc",
            SessionStatus::Exited,
            std::time::Duration::from_secs(30),
        );

        let reopened = core.activate("svc").expect("an ended session opens again");

        assert!(
            reopened.started,
            "nothing was running, so this created the run"
        );
        assert_eq!(reopened.runtime.status, SessionStatus::Running);
        core.stop("svc").expect("cleanup");
    }

    /// Opening a session that does not exist says so, by name.
    #[test]
    fn activating_an_unknown_session_is_refused() {
        let core = SessionCore::without_listener();

        let error = core
            .activate("nobody")
            .expect_err("there is no such session");

        assert_eq!(error.kind, SessionErrorKind::UnknownSession);
        assert_eq!(error.operation, "activate");
        assert!(error.message.contains("nobody"), "{}", error.message);
    }

    /// The registration a failed save has to take back (#64): it goes away
    /// without a word, because nothing ever told a listener it existed.
    #[test]
    fn a_registration_can_be_taken_back_before_it_is_announced() {
        let (core, sink) = core_with("svc");
        let events = sink.events().len();

        assert!(
            core.unregister("svc"),
            "a registered session can be taken back"
        );
        assert!(core.snapshot("svc").is_none());
        assert!(
            !core.unregister("svc"),
            "taking it back twice is not a second removal"
        );
        assert_eq!(
            sink.events().len(),
            events,
            "nothing announced a session nobody was told about"
        );
    }

    /// A session holding a run is never taken back, whatever the caller wants:
    /// its handle is the only thing accounting for the process tree (#61).
    #[test]
    fn a_session_that_owns_a_run_is_not_taken_back() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");

        assert!(
            !core.unregister("svc"),
            "a live run is not dropped by unregister"
        );

        assert!(core.snapshot("svc").is_some(), "the session is still there");
        core.stop("svc").expect("cleanup");
    }

    /// Spec §5 rule 4, and T04's acceptance criterion: a run that ends on its
    /// own is reflected in session state, with no caller asking and no UI
    /// involved.
    #[test]
    fn a_run_that_ends_on_its_own_updates_the_session_without_being_asked() {
        let (core, sink) = core_with_command("svc", "cmd.exe /c ping -n 2 127.0.0.1");
        core.start("svc").expect("start succeeds");

        let runtime = wait_for_status(
            &core,
            "svc",
            SessionStatus::Exited,
            std::time::Duration::from_secs(30),
        );

        assert_eq!(runtime.pid, None, "nothing is running any more");
        assert_eq!(
            runtime.exit_code,
            Some(0),
            "the run's own exit code is what gets recorded"
        );
        assert_eq!(
            runtime.last_error, None,
            "ending on its own is not an error"
        );
        assert_eq!(core.summary().running, 0);

        let record = sink
            .events()
            .into_iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::RunRecordUpdated(inner) => Some(inner.run),
                _ => None,
            })
            .expect("the ending was published");
        assert_eq!(record.exit_code, Some(0));
        assert!(record.ended_at.is_some(), "the run record is closed");
    }

    /// Spec §5 lists `Running -> Error` as well as `Running -> Exited`, so the
    /// two must not be the same outcome. A service that falls over on its own
    /// has failed; reporting it as a clean `Exited` would leave
    /// `AppSummary.error` at zero and give the tray's "Restart Failed" nothing
    /// to key on (spec §11).
    #[test]
    fn a_run_that_fails_on_its_own_ends_in_error_not_exited() {
        let (core, _sink) = core_with_command("svc", "cmd.exe /c exit 3");
        core.start("svc").expect("start succeeds");

        let runtime = wait_for_status(
            &core,
            "svc",
            SessionStatus::Error,
            std::time::Duration::from_secs(30),
        );

        assert_eq!(runtime.exit_code, Some(3), "the failing code is kept");
        assert_eq!(runtime.pid, None);
        assert!(
            runtime.last_error.is_some(),
            "a failure has to carry a reason the UI can show"
        );
        assert_eq!(core.summary().error, 1);
        assert_eq!(core.summary().running, 0);
    }

    /// A run that was stopped on purpose has already been accounted for. The
    /// watcher waking up afterwards must not publish a second ending, which
    /// would show a session stopping twice in the UI.
    #[test]
    fn a_stopped_run_is_not_reported_again_by_its_watcher() {
        let (core, sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");
        let stopped = core.force_stop("svc").expect("force stop succeeds");
        assert_eq!(stopped.status, SessionStatus::Exited);

        let settled = sink.events().len();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(
            sink.events().len(),
            settled,
            "the watcher published after the stop: {:?}",
            sink.names()
        );
        assert_eq!(
            core.snapshot("svc").expect("registered").status,
            SessionStatus::Exited
        );
    }

    /// Spec §5 rule 5 keeps the exit context in the run record, and §9 has a
    /// `run-record-updated` event to carry it. A stop the user asked for is
    /// still a run ending, so it has to be announced: the watcher refuses to
    /// report an ending a stop already owns, which would otherwise leave the
    /// closing of the record unpublished for every stop and force-stop.
    #[test]
    fn stopping_publishes_the_closed_run_record() {
        let (core, sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");

        core.force_stop("svc").expect("force stop succeeds");

        let closed = sink
            .events()
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::RunRecordUpdated(inner) => Some(inner.run),
                _ => None,
            })
            .next_back()
            .expect("the ending was announced");

        assert!(closed.ended_at.is_some(), "the run record is closed");
        assert!(closed.exit_code.is_some(), "the exit code is recorded");
    }

    #[test]
    fn stopping_a_running_service_ends_it_and_records_the_exit_code() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");

        let runtime = core
            .stop_with_timeout("svc", std::time::Duration::from_secs(2))
            .expect("stop succeeds");

        assert!(
            matches!(
                runtime.status,
                SessionStatus::Stopped | SessionStatus::Exited
            ),
            "a stopped run ends Stopped or Exited, saw {}",
            runtime.status.as_str()
        );
        assert_eq!(runtime.pid, None, "nothing is running any more");
        assert!(
            runtime.exit_code.is_some(),
            "the termination result is observable"
        );
        assert_eq!(core.summary().running, 0);
    }

    #[test]
    fn force_stop_ends_the_run_without_waiting_for_it() {
        let (core, _sink) = core_with_command("svc", LONG_RUNNING);
        core.start("svc").expect("start succeeds");

        let runtime = core.force_stop("svc").expect("force stop succeeds");

        assert_eq!(runtime.status, SessionStatus::Exited);
        assert_eq!(runtime.pid, None);
        assert!(runtime.exit_code.is_some());
        assert_eq!(core.summary().running, 0);
    }

    /// Restarting a stopped session has to work, and stopping one that is not
    /// running has to be a clear refusal rather than a silent success.
    #[test]
    fn stopping_a_session_that_is_not_running_is_refused() {
        let (core, _sink) = core_with("svc");

        let error = core.stop("svc").expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::InvalidTransition);
        assert_eq!(error.from, Some(SessionStatus::Stopped));
        assert!(error.message.contains("stopped"), "message: {error}");

        let forced = core.force_stop("svc").expect_err("refused");
        assert_eq!(forced.kind, SessionErrorKind::InvalidTransition);
    }

    /// A refused operation must not have moved the session.
    #[test]
    fn a_refused_start_leaves_the_session_untouched() {
        let (core, sink) = core_with("svc");

        let error = core
            .stop("svc")
            .expect_err("stopping a stopped session is refused");
        assert_eq!(error.kind, SessionErrorKind::InvalidTransition);

        let runtime = core.snapshot("svc").expect("registered");
        assert_eq!(runtime.status, SessionStatus::Stopped);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.run_id, None);
        assert!(
            sink.names().is_empty(),
            "a refusal publishes nothing, saw {:?}",
            sink.names()
        );
    }

    #[test]
    fn a_registered_session_is_stopped_with_its_configured_logging() {
        let (core, _sink) = core_with("svc");

        let runtime = core.snapshot("svc").expect("session is registered");
        assert_eq!(runtime.status, SessionStatus::Stopped);
        assert_eq!(runtime.pid, None, "no run exists yet");
        assert_eq!(runtime.run_id, None, "no run exists yet");
        assert_eq!(runtime.logging.mode, EffectiveLogMode::Always);
        assert_eq!(runtime.logging.source, LogSource::Captured);
    }

    #[test]
    fn an_unknown_session_has_no_snapshot() {
        let (core, _sink) = core_with("svc");
        assert!(core.snapshot("nope").is_none());
    }

    #[test]
    fn registering_the_same_id_twice_is_refused() {
        let (core, _sink) = core_with("svc");

        let error = core
            .register(service("svc"))
            .expect_err("duplicate is refused");
        assert_eq!(error.kind, SessionErrorKind::AlreadyRegistered);
        assert_eq!(error.session_id, "svc");
        assert!(error.message.contains("svc"), "message names it: {error}");
    }

    #[test]
    fn the_summary_counts_registered_sessions() {
        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink);
        core.register(service("a")).expect("registration succeeds");
        core.register(service("b")).expect("registration succeeds");

        let summary = core.summary();
        assert_eq!(summary.total, 2);
        assert_eq!(summary.running, 0, "nothing has been started");
        assert_eq!(summary.error, 0);
    }

    #[test]
    fn registration_does_not_publish_a_lifecycle_event() {
        // Registering is not a lifecycle transition; a listener should not see
        // a state change for a session that has not moved.
        let (core, sink) = core_with("svc");
        assert!(sink.names().is_empty(), "saw {:?}", sink.names());

        let _ = core.snapshot("svc");
        assert!(sink.names().is_empty());
    }

    /// The command string is user-authored, so the cases that matter are the
    /// ones where splitting could quietly produce the wrong program or the
    /// wrong argument boundary.
    #[test]
    fn a_command_splits_into_a_program_and_arguments() {
        let (program, args) = split_command("node server.js --port 8000").expect("splits");
        assert_eq!(program, PathBuf::from("node"));
        assert_eq!(args, vec!["server.js", "--port", "8000"]);
    }

    #[test]
    fn a_quoted_program_keeps_its_spaces_and_extra_whitespace_is_ignored() {
        let (program, args) =
            split_command("  \"C:\\Program Files\\node\\node.exe\"   server.js  ").expect("splits");
        assert_eq!(program, PathBuf::from("C:\\Program Files\\node\\node.exe"));
        assert_eq!(args, vec!["server.js"]);
    }

    /// A quoted argument keeps its inner spacing without becoming two
    /// arguments, which is the whole reason quotes are honoured.
    #[test]
    fn a_quoted_argument_stays_one_argument() {
        let (program, args) = split_command("app.exe --title \"My Service\"").expect("splits");
        assert_eq!(program, PathBuf::from("app.exe"));
        assert_eq!(args, vec!["--title", "My Service"]);
    }

    #[test]
    fn an_empty_quoted_argument_is_still_an_argument() {
        let (program, args) = split_command("app.exe \"\"").expect("splits");
        assert_eq!(program, PathBuf::from("app.exe"));
        assert_eq!(args, vec![""]);
    }

    /// Refused rather than guessed at: a mis-split command would start the
    /// wrong program, and the user would have no idea why.
    #[test]
    fn an_unbalanced_quote_is_refused() {
        let error = split_command("app.exe \"unterminated").expect_err("refused");
        assert!(error.contains("quote"), "message: {error}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(split_command("   ").is_err());
        assert!(split_command("").is_err());
    }

    #[test]
    fn a_service_session_maps_to_a_supervised_process() {
        let mut config = service("svc");
        config.cwd = Some(PathBuf::from("C:\\work"));

        let spec =
            process_spec(&config, "svc", &planned_for(&config)).expect("a service has a spec");
        assert_eq!(spec.program, PathBuf::from("cmd.exe"));
        assert_eq!(spec.args, vec!["/c", "exit", "0"]);
        assert_eq!(spec.cwd, PathBuf::from("C:\\work"));
    }

    /// `cwd` is optional in the schema but cannot be defaulted predictably, so
    /// the refusal has to name what is missing.
    #[test]
    fn a_service_without_a_working_directory_is_refused_with_a_reason() {
        let mut config = service("svc");
        config.cwd = None;

        let error = process_spec(&config, "svc", &planned_for(&config)).expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::Failed);
        assert!(error.message.contains("cwd"), "message: {}", error.message);
    }

    /// The seam between a configured session and the run it gets: a service is
    /// supervised (T03), an interactive terminal is hosted on a PTY (T02,
    /// wired here by T07 #8). The dispatch is by session type, so neither
    /// builder can be asked to host something it does not own.
    #[test]
    fn a_terminal_is_hosted_on_a_terminal_and_a_service_on_a_supervised_process() {
        let mut terminal = service("term");
        terminal.session_type = SessionType::Terminal;
        terminal.command = None;
        terminal.shell = Some("powershell".to_owned());
        terminal.cwd = Some(PathBuf::from("C:\\work"));

        match start_spec(&terminal, None, "term", &planned_for(&terminal)).expect("a terminal spec")
        {
            StartSpec::Terminal(spec) => {
                assert_eq!(spec.program, PathBuf::from("powershell"));
                assert_eq!(spec.cwd, PathBuf::from("C:\\work"));
                assert_eq!(
                    (spec.cols, spec.rows),
                    (DEFAULT_COLS, DEFAULT_ROWS),
                    "a view that has not measured itself gets the PTY default"
                );
            }
            other => panic!("a terminal must not be hosted by another layer: {other:?}"),
        }

        let service = service("svc");
        match start_spec(&service, None, "svc", &planned_for(&service)).expect("a service spec") {
            StartSpec::Process(spec) => assert_eq!(spec.program, PathBuf::from("cmd.exe")),
            other => panic!("a Hub-internal service must not be hosted elsewhere: {other:?}"),
        }
    }

    /// A view measures itself before its session starts, and that measurement
    /// is what the shell is born with (T07): a shell started at the default and
    /// corrected a moment later draws its first screen at the wrong width.
    #[test]
    fn a_terminal_is_hosted_at_the_geometry_its_view_asked_for() {
        let mut terminal = service("term");
        terminal.session_type = SessionType::Terminal;
        terminal.command = None;
        terminal.shell = Some("powershell".to_owned());
        terminal.cwd = Some(PathBuf::from("C:\\work"));

        match start_spec(&terminal, Some((120, 30)), "term", &planned_for(&terminal))
            .expect("a terminal spec")
        {
            StartSpec::Terminal(spec) => assert_eq!((spec.cols, spec.rows), (120, 30)),
            other => panic!("a terminal must not be hosted by another layer: {other:?}"),
        }
    }

    /// A terminal's `shell` is required for the same reason a service's
    /// `command` is: the refusal names what is missing rather than hosting
    /// something arbitrary.
    #[test]
    fn a_terminal_without_a_shell_is_refused_with_a_reason() {
        let mut config = service("term");
        config.session_type = SessionType::Terminal;
        config.command = None;
        config.shell = None;

        let error = start_spec(&config, None, "term", &planned_for(&config)).expect_err("refused");
        assert_eq!(error.kind, SessionErrorKind::Failed);
        assert_eq!(error.session_id, "term");
        assert!(
            error.message.contains("shell"),
            "message: {}",
            error.message
        );
    }

    /// The pipe decision is made from the resolved policy, not from the
    /// session type: a `captured` service has its output taken (its scrollback
    /// is filled even when nothing is written), and one linked to the
    /// application's own log does not (D-005).
    #[test]
    fn a_run_is_piped_only_when_its_policy_captures() {
        let captured = service("svc");
        let mut external = service("app");
        external.logging = EffectiveLogging {
            mode: EffectiveLogMode::Always,
            source: LogSource::External,
            external_path: Some("D:/app/access.log".to_owned()),
        };

        let piped = process_spec(&captured, "svc", &planned_for(&captured)).expect("a spec");
        let linked = process_spec(&external, "app", &planned_for(&external)).expect("a spec");

        assert_eq!(piped.output, OutputMode::Capture);
        assert_eq!(linked.output, OutputMode::Inherit);
    }

    // ---- T05 (#6): logging ------------------------------------------------

    mod logging {
        use super::*;
        use crate::config::{EffectiveLogMode, EffectiveLogging, LogSource};
        use crate::logging::test_support::TempDir;
        use crate::logging::LogRoots;
        use std::fs;
        use std::path::Path as FsPath;

        /// A core that writes under a scratch app-data root, with one session
        /// registered.
        fn core_logging_to(
            dir: &TempDir,
            config: SessionConfig,
        ) -> (SessionCore, Arc<RecordingSink>) {
            let sink = Arc::new(RecordingSink::default());
            let core = SessionCore::new(sink.clone()).with_log_roots(LogRoots {
                logs_dir: dir.join("logs"),
                metadata_dir: dir.join("metadata"),
            });
            core.register(config).expect("registration succeeds");
            (core, sink)
        }

        fn captured(mode: EffectiveLogMode) -> EffectiveLogging {
            EffectiveLogging {
                mode,
                source: LogSource::Captured,
                external_path: None,
            }
        }

        fn service_printing(id: &str, command: &str, logging: EffectiveLogging) -> SessionConfig {
            let mut config = service(id);
            config.command = Some(command.to_owned());
            config.logging = logging;
            config
        }

        /// Poll `check` until it holds, so a test asserts on what the pumping
        /// and watching threads produced rather than on how long they took.
        fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !check() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        fn read(path: &FsPath) -> String {
            fs::read_to_string(path).unwrap_or_default()
        }

        /// Wait until the ended run has been filed: its record on disk, which
        /// is what a log action resolves through.
        ///
        /// `Exited` is set before the ending is finalised — the state moves,
        /// then the run's pipes are drained, its log closed and its record
        /// written — so a test that read the target the moment it saw `Exited`
        /// would race the very step it is asserting on.
        fn wait_for_filing(core: &SessionCore, session_id: &str) {
            wait_until("the ended run to be filed", || {
                !core.run_history(session_id).runs.is_empty()
            });
        }

        /// The run's log file, named the moment the run starts.
        fn announced_log(core: &SessionCore, session_id: &str) -> PathBuf {
            let status = core.log_status(session_id).expect("the session exists");
            PathBuf::from(
                status
                    .log_file
                    .expect("the run's file is named while it runs"),
            )
        }

        /// T05's central criterion for a service: `captured` + `always` puts
        /// the run's output in a run-specific file, and the run history is how
        /// that file is found again (`docs/LOGGING.md` §6).
        #[test]
        fn a_captured_run_writes_a_file_its_run_record_points_at() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo listening-on-8188",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);

            let runtime = core.start("svc").expect("start succeeds");
            let file = announced_log(&core, "svc");
            assert!(file.exists(), "the run file is created when the run starts");

            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the run's output to reach its file", || {
                read(&file).contains("listening-on-8188")
            });

            let history = core.run_history("svc");
            let run = &history.latest().expect("the run is in the history").run;
            assert_eq!(Some(run.run_id.clone()), runtime.run_id);
            assert_eq!(
                run.log_file.as_deref(),
                Some(file.to_string_lossy().as_ref()),
                "the history must name the file the run wrote"
            );
            assert!(
                PathBuf::from(run.log_file.clone().expect("a log file")).exists(),
                "the file the record names does not exist"
            );
            assert_eq!(run.exit_code, Some(0));
            assert!(run.ended_at.is_some(), "the run is closed");
        }

        /// Both streams are captured, and the file says which one each part
        /// came from (`docs/LOGGING.md` §7).
        #[test]
        fn a_run_file_carries_both_streams_tagged() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo to-stdout & echo to-stderr 1>&2",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("svc").expect("start succeeds");
            let file = announced_log(&core, "svc");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("both streams to reach the file", || {
                let text = read(&file);
                text.contains("to-stdout") && text.contains("to-stderr")
            });

            let text = read(&file);
            assert!(
                text.contains("to-stdout") && text.contains("stdout"),
                "stdout is untagged: {text}"
            );
            assert!(
                text.contains("to-stderr") && text.contains("stderr"),
                "stderr is untagged: {text}"
            );
        }

        /// §8 and D-004 together: `off` writes nothing anywhere, and the
        /// session still has its bounded scrollback.
        #[test]
        fn an_off_run_leaves_no_file_and_still_has_a_scrollback() {
            let dir = TempDir::new();
            let config = service_printing(
                "term",
                "cmd.exe /c echo interactive-output",
                EffectiveLogging {
                    mode: EffectiveLogMode::Off,
                    source: LogSource::None,
                    external_path: None,
                },
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("term").expect("start succeeds");
            wait_for_status(
                &core,
                "term",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the scrollback to fill", || {
                core.snapshot("term")
                    .expect("the session exists")
                    .buffer
                    .bytes
                    > 0
            });

            assert!(
                !dir.join("logs").exists(),
                "an `off` session created a log directory"
            );
            let status = core.log_status("term").expect("status");
            assert_eq!(status.state, crate::logging::LogState::Off);
            assert!(status.log_file.is_none());
            assert!(
                !status.records_input,
                "user input is never recorded by default"
            );

            let chunks = core.terminal_buffer("term").expect("a buffer");
            let text: String = chunks.iter().map(|chunk| chunk.text()).collect();
            assert!(
                text.contains("interactive-output"),
                "the scrollback lost the output: {text}"
            );
            // The record is filed just after the state changes, so the history
            // is polled rather than assumed to be there the moment the session
            // reads `Exited`.
            wait_until("the run to be filed", || {
                !core.run_history("term").runs.is_empty()
            });
            assert!(
                core.run_history("term")
                    .latest()
                    .expect("the run is filed")
                    .run
                    .log_file
                    .is_none(),
                "an `off` run's record must name no file"
            );
        }

        /// §13 scenario C: a clean run leaves nothing, a failed run keeps the
        /// context from before the failure.
        #[test]
        fn on_error_keeps_the_context_of_a_failing_run_only() {
            let dir = TempDir::new();
            let clean = service_printing(
                "clean",
                "cmd.exe /c echo all-good",
                captured(EffectiveLogMode::OnError),
            );
            let (core, _sink) = core_logging_to(&dir, clean);
            core.start("clean").expect("start succeeds");
            wait_for_status(
                &core,
                "clean",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the clean run to be filed", || {
                !core.run_history("clean").runs.is_empty()
            });
            assert!(
                core.run_history("clean")
                    .latest()
                    .expect("a run record")
                    .run
                    .log_file
                    .is_none(),
                "a clean `on_error` run must not name a file"
            );

            let failing = service_printing(
                "failing",
                "cmd.exe /c echo before-the-crash & exit 3",
                captured(EffectiveLogMode::OnError),
            );
            let (core, _sink) = core_logging_to(&dir, failing);
            core.start("failing").expect("start succeeds");
            wait_for_status(
                &core,
                "failing",
                SessionStatus::Error,
                std::time::Duration::from_secs(30),
            );
            wait_until("the failure's log to appear", || {
                core.run_history("failing")
                    .latest()
                    .map(|entry| &entry.run)
                    .and_then(|run| run.log_file.clone())
                    .map(|path| read(FsPath::new(&path)).contains("before-the-crash"))
                    .unwrap_or(false)
            });

            let history = core.run_history("failing");
            let run = &history.latest().expect("a run").run;
            assert_eq!(run.exit_code, Some(3));
            let text = read(FsPath::new(run.log_file.as_deref().expect("a log file")));
            assert!(
                text.contains("exit code 3"),
                "the log does not say how the run ended: {text}"
            );
        }

        /// D-005: an application-owned log is linked, not duplicated. The Hub
        /// must not capture the output and must not write a file of its own,
        /// while the run record still points a user at the application's file.
        #[test]
        fn an_external_session_links_the_application_log_and_captures_nothing() {
            let dir = TempDir::new();
            let external = dir.join("app/access.log").display().to_string();
            let config = service_printing(
                "app",
                "cmd.exe /c echo output-the-app-logs-itself",
                EffectiveLogging {
                    mode: EffectiveLogMode::Always,
                    source: LogSource::External,
                    external_path: Some(external.clone()),
                },
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("app").expect("start succeeds");
            wait_for_status(
                &core,
                "app",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the run to be filed", || {
                !core.run_history("app").runs.is_empty()
            });

            assert!(
                !dir.join("logs").exists(),
                "an external session must not write a Hub log"
            );
            let status = core.log_status("app").expect("status");
            assert_eq!(status.state, crate::logging::LogState::External);
            assert_eq!(status.external_log.as_deref(), Some(external.as_str()));
            assert_eq!(status.log_file.as_deref(), Some(external.as_str()));
            assert_eq!(
                core.run_history("app")
                    .latest()
                    .expect("a run")
                    .run
                    .log_file
                    .as_deref(),
                Some(external.as_str())
            );
        }

        /// A session that has never run still answers "am I being recorded?" —
        /// including the answer "I cannot be, and here is why".
        #[test]
        fn a_session_with_nowhere_to_write_says_why_it_is_not_logging() {
            let (core, _sink) = core_with_command("svc", "cmd.exe /c exit 0");
            core.start("svc").expect("start succeeds");

            let status = core.log_status("svc").expect("status");

            assert_eq!(status.mode, EffectiveLogMode::Always);
            assert_eq!(status.source, LogSource::Captured);
            assert!(status.log_file.is_none(), "nothing can be written");
            let problem = status.last_error.expect("the reason is reported");
            assert!(
                problem.message.contains("writable log directory"),
                "unhelpful message: {}",
                problem.message
            );
            core.force_stop("svc").expect("cleanup");
        }

        /// A terminal session's default policy is `off`/`none` (T01's `auto`
        /// resolution), and its status has to say so before it ever runs —
        /// T07 attaches the PTY, and the UI shows the policy meanwhile.
        #[test]
        fn a_terminal_session_reports_its_policy_before_it_has_ever_run() {
            let dir = TempDir::new();
            let mut config = service("shell");
            config.session_type = SessionType::Terminal;
            config.command = None;
            config.shell = Some("powershell".to_owned());
            config.logging = EffectiveLogging {
                mode: EffectiveLogMode::Off,
                source: LogSource::None,
                external_path: None,
            };
            let (core, _sink) = core_logging_to(&dir, config);

            let status = core.log_status("shell").expect("status");

            assert_eq!(status.state, crate::logging::LogState::Off);
            assert!(status.log_file.is_none());
            assert!(!status.records_input);
            assert!(
                status.session_log_dir.is_some(),
                "the folder a run would write into is known before any run"
            );
        }

        /// §3's "save this run's log": an `on_error` run can be committed while
        /// it is still going, and it keeps its file even if it later exits
        /// cleanly — the user asked for it.
        #[test]
        fn saving_a_running_on_error_log_commits_it() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c ping -n 120 127.0.0.1",
                captured(EffectiveLogMode::OnError),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");

            let status = core.save_run_log("svc").expect("the save succeeds");

            let file = PathBuf::from(status.log_file.expect("a file was committed"));
            assert!(file.exists(), "the saved log does not exist");
            core.force_stop("svc").expect("cleanup");
            assert!(file.exists(), "a forced stop deleted the saved log");
        }

        /// §3's `manual` mode: nothing is recorded until the user asks, and
        /// switching recording on is refused for a mode that does not have the
        /// question.
        #[test]
        fn manual_recording_is_switched_on_and_off_by_request_only() {
            let dir = TempDir::new();
            let manual = service_printing(
                "shell",
                "cmd.exe /c ping -n 120 127.0.0.1",
                captured(EffectiveLogMode::Manual),
            );
            let (core, _sink) = core_logging_to(&dir, manual);
            core.start("shell").expect("start succeeds");

            let idle = core.log_status("shell").expect("status");
            assert_eq!(
                idle.state,
                crate::logging::LogState::Off,
                "manual records nothing until asked"
            );

            let recording = core
                .set_log_recording("shell", true)
                .expect("recording starts");
            assert_eq!(recording.state, crate::logging::LogState::Capturing);
            let file = PathBuf::from(recording.log_file.expect("a file is open"));
            assert!(file.exists());

            let stopped = core
                .set_log_recording("shell", false)
                .expect("recording stops");
            assert_eq!(stopped.state, crate::logging::LogState::Off);

            core.force_stop("shell").expect("cleanup");

            let always = service_printing(
                "svc",
                "cmd.exe /c ping -n 120 127.0.0.1",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, always);
            core.start("svc").expect("start succeeds");
            let refused = core
                .set_log_recording("svc", true)
                .expect_err("only manual sessions can be switched");
            assert!(
                refused.message.contains("manual"),
                "unhelpful message: {}",
                refused.message
            );
            core.force_stop("svc").expect("cleanup");
        }

        /// The scrollback is bounded per session, not per run: restarting does
        /// not throw away what the user was reading (§8), and the snapshot
        /// reports how much of it there is without carrying it.
        #[test]
        fn the_scrollback_survives_a_restart_and_stays_bounded() {
            let dir = TempDir::new();
            // Longer than the 64-byte scrollback below, so the bound is
            // reached on the first run.
            let noise = "0123456789".repeat(20);
            let config = service_printing(
                "svc",
                &format!("cmd.exe /c echo {noise}"),
                captured(EffectiveLogMode::Off),
            );
            let sink = Arc::new(RecordingSink::default());
            let core = SessionCore::new(sink)
                .with_log_roots(LogRoots {
                    logs_dir: dir.join("logs"),
                    metadata_dir: dir.join("metadata"),
                })
                .with_limits(
                    crate::logging::BufferLimits::new(64, usize::MAX),
                    crate::logging::DEFAULT_LOG_LIMITS,
                );
            core.register(config).expect("registration succeeds");

            core.start("svc").expect("start succeeds");
            wait_until("the first run's output", || {
                core.snapshot("svc").expect("the session").buffer.bytes > 0
            });
            let after_first = core.snapshot("svc").expect("the session").buffer;

            assert!(
                after_first.bytes <= 64,
                "the scrollback is not bounded: {} bytes",
                after_first.bytes
            );
            assert!(
                after_first.dropped_bytes > 0,
                "output was silently dropped without being counted"
            );

            // A second run of the same session: the buffer belongs to the
            // session, so what it already held is still there and its counters
            // carry on rather than starting again.
            core.restart("svc").expect("restart succeeds");
            wait_until("the second run's output", || {
                core.snapshot("svc")
                    .expect("the session")
                    .buffer
                    .dropped_bytes
                    > after_first.dropped_bytes
            });

            let after_second = core.snapshot("svc").expect("the session").buffer;
            assert!(
                after_second.bytes <= 64,
                "the scrollback is not bounded after a restart: {} bytes",
                after_second.bytes
            );
        }

        /// §9's cleanup, through the entry point that owns it: a sweep never
        /// takes a file that is still inside the retention window, and never
        /// reaches outside the Hub's own log root.
        #[test]
        fn a_cleanup_leaves_logs_inside_the_retention_window_alone() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo keep-me",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            let file = announced_log(&core, "svc");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );

            let report = core.cleanup_logs(Some("svc"));

            assert_eq!(report.removed_count(), 0, "{:?}", report.removed);
            assert!(file.exists(), "a fresh run's log was deleted");
        }

        /// A `manual` session the user recorded and then stopped still has a
        /// log, and the run record has to name it — that is what makes the file
        /// findable afterwards (`docs/LOGGING.md` §6).
        #[test]
        fn a_stopped_manual_recording_still_names_its_file() {
            let dir = TempDir::new();
            let manual = service_printing(
                "shell",
                "cmd.exe /c ping -n 120 127.0.0.1",
                captured(EffectiveLogMode::Manual),
            );
            let (core, _sink) = core_logging_to(&dir, manual);
            core.start("shell").expect("start succeeds");
            let status = core
                .set_log_recording("shell", true)
                .expect("recording starts");
            let file = PathBuf::from(status.log_file.expect("a file is open"));

            core.set_log_recording("shell", false)
                .expect("recording stops");
            core.force_stop("shell").expect("cleanup");

            wait_until("the run to be filed", || {
                !core.run_history("shell").runs.is_empty()
            });
            assert_eq!(
                core.run_history("shell")
                    .latest()
                    .expect("a run")
                    .run
                    .log_file
                    .as_deref(),
                Some(file.to_string_lossy().as_ref()),
                "the record lost the file the user recorded"
            );
            assert!(file.exists());
        }

        /// Stopping an `on_error` session on purpose is not a failure. The
        /// terminated process reports a non-zero code, and a log written for
        /// every press of Stop is exactly the litter `docs/LOGGING.md` §1.2 is
        /// about.
        #[test]
        fn stopping_an_on_error_run_on_purpose_writes_no_log() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c ping -n 120 127.0.0.1",
                captured(EffectiveLogMode::OnError),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Running,
                std::time::Duration::from_secs(30),
            );

            core.force_stop("svc").expect("cleanup");

            let history = core.run_history("svc");
            assert_eq!(
                history.latest().expect("a run").run.log_file,
                None,
                "a requested stop filed an error log"
            );
            assert!(
                !dir.join("logs").exists(),
                "a requested stop wrote a log directory"
            );
        }

        /// A record names a log file only when one exists (spec §4). A run
        /// whose log directory cannot be written is still filed — the run
        /// happened — but it must not point at a path that does not exist.
        #[test]
        fn a_run_whose_log_cannot_be_written_names_no_file() {
            let dir = TempDir::new();
            // A file where the `logs` directory would have to be.
            fs::write(dir.join("logs"), b"not a directory").expect("the blocker is writable");
            let config = service_printing(
                "svc",
                "cmd.exe /c echo nowhere-to-write",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the run to be filed", || {
                !core.run_history("svc").runs.is_empty()
            });

            let status = core.log_status("svc").expect("status");
            assert!(status.log_file.is_none(), "a file that does not exist");
            assert!(
                status.last_error.is_some(),
                "the failure to write has to be reported somewhere"
            );
            assert!(core
                .run_history("svc")
                .latest()
                .expect("a run")
                .run
                .log_file
                .is_none());
        }

        /// A run that ends before its output is read is still captured: the
        /// pipe holds the bytes, and the run is not over until they have been
        /// taken (`docs/LOGGING.md` §3).
        #[test]
        fn a_run_that_exits_immediately_still_gets_its_output_written() {
            let dir = TempDir::new();
            // Nothing that waits: the process writes and exits at once.
            let config = service_printing(
                "svc",
                "cmd.exe /c echo gone-already",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("svc").expect("start succeeds");
            let file = announced_log(&core, "svc");

            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the short run's output to reach its file", || {
                read(&file).contains("gone-already")
            });
        }

        // ---- T10 (#11): resolving a log action to a file -------------------

        /// "Open this session's log" is resolved by Core, and what it resolves
        /// to is the file the run history names — not a path the caller handed
        /// in (`docs/LOGGING.md` §10).
        #[test]
        fn opening_a_log_opens_the_file_its_run_record_names() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo open-me",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_for_filing(&core, "svc");

            let file = core.log_file_path("svc", None).expect("a path");
            let run = &core
                .run_history("svc")
                .latest()
                .cloned()
                .expect("a run")
                .run;

            assert_eq!(
                file,
                PathBuf::from(run.log_file.clone().expect("the record names a file")),
                "the path is not the file the history points at"
            );
            assert!(file.exists(), "the resolved file is not on disk");
        }

        /// A run named explicitly resolves to *that* run's file, so opening a
        /// file from the history opens the one the user clicked.
        #[test]
        fn a_named_run_resolves_to_its_own_file() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo first-run",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);

            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_for_filing(&core, "svc");
            let first = &core
                .run_history("svc")
                .latest()
                .cloned()
                .expect("a run")
                .run;

            // A second run replaces the current file; the first one stays.
            core.start("svc").expect("restart succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_until("the second run to be filed", || {
                core.run_history("svc").runs.len() >= 2
            });
            let newest = core.log_file_path("svc", None).expect("a path");

            let older = core
                .log_file_path("svc", Some(first.run_id.as_str()))
                .expect("the first run is still in the history");

            assert_eq!(
                older,
                PathBuf::from(first.log_file.clone().expect("a file"))
            );
            assert_ne!(older, newest, "both runs resolved to one file");
        }

        /// A run that has left nothing on disk says so, and says what to do.
        ///
        /// This is §1.4's real test: the user asked for the log of a run that
        /// is still healthy under `on_error`, and the answer must not be a
        /// silent success or a bare failure but the way to get the file.
        #[test]
        fn a_run_with_nothing_persisted_explains_how_to_save_it() {
            let dir = TempDir::new();
            let config = service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::OnError));
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");

            let error = core
                .log_file_path("svc", None)
                .expect_err("a healthy on_error run has no file yet");

            assert_eq!(error.kind, SessionErrorKind::Failed);
            assert_eq!(error.operation, "open_log_file");
            assert!(
                error.message.contains("save_run_log"),
                "the message does not say how to get the file: {}",
                error.message
            );

            // And the action it names works, exactly as the message promised.
            let status = core.save_run_log("svc").expect("the buffer can be saved");
            let file = core
                .log_file_path("svc", None)
                .expect("now there is a file");
            assert_eq!(file, PathBuf::from(status.log_file.expect("a file")));
            assert!(file.exists());

            core.force_stop("svc").expect("cleanup");
        }

        /// An `external` session's log belongs to the application, so every
        /// action points at that file and never at anything the Hub wrote
        /// (`docs/DECISIONS.md` D-005).
        #[test]
        fn an_external_session_points_at_the_application_own_log() {
            let dir = TempDir::new();
            let mut config =
                service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::Always));
            config.logging = EffectiveLogging {
                mode: EffectiveLogMode::Always,
                source: LogSource::External,
                external_path: Some("D:/Tools/SillyTavern/data/access.log".to_owned()),
            };
            // Registered, never started: the link exists whether or not the Hub
            // has ever run the session.
            let (core, _sink) = core_logging_to(&dir, config);

            let file = core.log_file_path("svc", None).expect("a path");
            let folder = core.log_folder_path("svc", None).expect("a folder");

            assert_eq!(file, PathBuf::from("D:/Tools/SillyTavern/data/access.log"));
            assert_eq!(folder, PathBuf::from("D:/Tools/SillyTavern/data"));
        }

        /// Naming something unknown is refused with the thing that was wrong,
        /// not with a generic failure.
        #[test]
        fn an_unknown_session_or_run_is_named_in_the_refusal() {
            let dir = TempDir::new();
            let config = service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::Always));
            let (core, _sink) = core_logging_to(&dir, config);

            let missing_session = core
                .log_file_path("nobody", None)
                .expect_err("no such session");
            assert_eq!(missing_session.kind, SessionErrorKind::UnknownSession);
            assert_eq!(missing_session.session_id, "nobody");

            let missing_run = core
                .log_file_path("svc", Some("deadbeef"))
                .expect_err("no such run");
            assert_eq!(missing_run.kind, SessionErrorKind::Failed);
            assert!(
                missing_run.message.contains("deadbeef"),
                "the refusal does not name the run: {}",
                missing_run.message
            );
        }

        /// The folder action answers with the folder that holds the file, which
        /// is what "where does this session write?" means
        /// (`docs/LOGGING.md` §5, D-011).
        #[test]
        fn the_folder_action_resolves_to_the_folder_holding_the_file() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo where-am-i",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_for_filing(&core, "svc");

            let file = core.log_file_path("svc", None).expect("a path");
            let folder = core.log_folder_path("svc", None).expect("a folder");

            assert_eq!(folder, file.parent().expect("a month folder").to_path_buf());
            assert!(folder.is_dir());
            assert!(
                folder.starts_with(dir.join("logs")),
                "the folder is not the Hub's own log layout: {}",
                folder.display()
            );
        }

        /// A preview describes a sweep and performs none of it, and the sweep
        /// that follows takes exactly what the preview named.
        ///
        /// The file is aged past the retention limit directly, because the
        /// shipped policy keeps 30 days and a test cannot wait for one.
        #[test]
        fn a_cleanup_preview_describes_the_sweep_without_making_it() {
            let dir = TempDir::new();
            let config = service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::Always));
            let (core, _sink) = core_logging_to(&dir, config);

            let month = dir.join("logs/svc/2026-09");
            fs::create_dir_all(&month).expect("the month folder is creatable");
            let stale = month.join("2026-09-24_09-30-15__run-old.log");
            let fresh = month.join("2026-09-29_09-30-15__run-new.log");
            fs::write(&stale, b"old output").expect("writable");
            fs::write(&fresh, b"new output").expect("writable");
            age_by_days(&stale, 45);

            let preview = core.cleanup_preview(Some("svc"));

            assert_eq!(preview.removed, vec![stale.clone()]);
            assert_eq!(preview.freed_bytes, "old output".len() as u64);
            assert!(stale.exists(), "a preview deleted the file it described");

            let swept = core.cleanup_logs(Some("svc"));

            assert_eq!(swept.removed, preview.removed);
            assert_eq!(swept.freed_bytes, preview.freed_bytes);
            assert!(swept.failures.is_empty(), "{:?}", swept.failures);
            assert!(!stale.exists());
            assert!(fresh.exists(), "the newest run's log was swept");
        }

        /// §6 and §9 together: a sweep takes the Hub's log files and never the
        /// record of the run that wrote them.
        ///
        /// The record is the evidence the run happened. Deleting it because its
        /// log aged out would trade the answer to "what has this session been
        /// doing" for tidiness, so the run stays in the history and says instead
        /// that the file it names is gone — which is what the Logs tab renders,
        /// rather than an action that fails when the user clicks it.
        #[test]
        fn a_swept_run_stays_in_the_history_without_its_log() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo sweep-me",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_for_filing(&core, "svc");

            let file = core.log_file_path("svc", None).expect("a path");
            assert!(
                core.run_history("svc")
                    .latest()
                    .expect("a run")
                    .log_file_present,
                "the run's log is not on disk to begin with"
            );

            // The run's own file has to become sweepable, and retention keeps
            // the newest file of a session however old it is: a later file
            // takes that place, and the run's own is aged past the limit the
            // way a month of not looking at it would.
            let later = file.with_file_name("2099-01-01_00-00-00__run-later.log");
            fs::write(&later, b"later output").expect("writable");
            age_by_days(&file, 45);

            let swept = core.cleanup_logs(Some("svc"));

            assert_eq!(swept.removed, vec![file.clone()]);
            assert!(!file.exists());
            let history = core.run_history("svc");
            assert_eq!(history.runs.len(), 1, "the sweep erased the run itself");
            let entry = history.latest().expect("the run is still in the history");
            assert!(
                !entry.log_file_present,
                "the row claims a file that was just deleted"
            );
            assert!(!entry.log_is_openable());
            assert_eq!(
                entry.run.log_file.as_deref(),
                Some(file.to_string_lossy().as_ref()),
                "the record must keep naming the file the run wrote"
            );
            assert!(
                !crate::logging::session_run_files(&dir.join("metadata"), "json", Some("svc"))
                    .is_empty(),
                "the sweep reached the run metadata"
            );
        }

        /// D-022 asked of the current run, not only of the history.
        ///
        /// The Logs tab's card offers the same file actions a history row does,
        /// so it has to answer "is the file there?" from the same read-time
        /// fact — otherwise one panel holds two rules for one question, and the
        /// card is the half that offers an action which fails when it is
        /// clicked.
        ///
        /// The file is deleted by hand rather than swept here: that is the
        /// other way a log goes missing (the OS does not ask the Hub first),
        /// and the answer has to be the same either way, because it is asked
        /// now and never remembered.
        #[test]
        fn the_current_run_reports_whether_its_log_is_still_on_disk() {
            let dir = TempDir::new();
            let config = service_printing(
                "svc",
                "cmd.exe /c echo current-file",
                captured(EffectiveLogMode::Always),
            );
            let (core, _sink) = core_logging_to(&dir, config);
            core.start("svc").expect("start succeeds");
            wait_for_status(
                &core,
                "svc",
                SessionStatus::Exited,
                std::time::Duration::from_secs(30),
            );
            wait_for_filing(&core, "svc");

            let status = core.log_status("svc").expect("a status");
            let file = status.log_file.clone().expect("the run named a file");
            assert!(
                status.log_file_present,
                "the file the run just wrote was reported gone"
            );
            assert!(FsPath::new(&file).exists());

            fs::remove_file(&file).expect("the log is removable");

            let gone = core.log_status("svc").expect("a status");
            assert_eq!(
                gone.log_file.as_deref(),
                Some(file.as_str()),
                "the status stopped naming the file the run wrote"
            );
            assert!(
                !gone.log_file_present,
                "a log deleted by hand was still answered as present"
            );
        }

        /// An `external` session's current file is the application's own
        /// (D-005), so the file answer is asked of *that* file.
        ///
        /// The Hub writes nothing for such a session, which is why its
        /// `log_file` is nil — reading the answer off that field would report
        /// every `external` session as having no log at all, including the ones
        /// whose application is writing one right now.
        #[test]
        fn an_external_sessions_file_answer_is_about_the_applications_file() {
            let dir = TempDir::new();
            let application_owned = dir.join("app/access.log");
            fs::create_dir_all(application_owned.parent().expect("a parent folder"))
                .expect("creatable");
            fs::write(&application_owned, b"the application's own output").expect("writable");

            let mut config = service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::Off));
            config.logging = EffectiveLogging {
                mode: EffectiveLogMode::Always,
                source: LogSource::External,
                external_path: Some(application_owned.display().to_string()),
            };
            let (core, _sink) = core_logging_to(&dir, config);

            let status = core.log_status("svc").expect("a status");
            assert_eq!(
                status.external_log.as_deref(),
                Some(application_owned.to_string_lossy().as_ref())
            );
            assert!(
                status.log_file.is_none(),
                "the status named a file the Hub does not write"
            );
            assert!(
                status.log_file_present,
                "the application's file is on disk and was reported gone"
            );

            fs::remove_file(&application_owned).expect("the application's log is removable");

            assert!(
                !core.log_status("svc").expect("a status").log_file_present,
                "a session whose application has not written its log still \
                 answered that there was one to open"
            );
        }

        /// §9's promise, kept even for a config that puts an application's log
        /// inside the Hub's own log tree.
        ///
        /// Location normally keeps it — an external log lives wherever its
        /// application put it — but nothing stops a `logging.path` from naming a
        /// file under `<logs>/<session>/<month>/`, where the walk would find it.
        /// The sweep is told which paths belong to an application, so the
        /// promise does not depend on where the user pointed the config.
        #[test]
        fn a_sweep_never_takes_an_application_owned_log() {
            let dir = TempDir::new();
            let month = dir.join("logs/svc/2026-09");
            fs::create_dir_all(&month).expect("the month folder is creatable");
            let application_owned = month.join("2026-09-20_09-30-15__app-access.log");
            let hub_stale = month.join("2026-09-24_09-30-15__run-old.log");
            let hub_current = month.join("2026-09-29_09-30-15__run-new.log");
            for path in [&application_owned, &hub_stale, &hub_current] {
                fs::write(path, b"output").expect("writable");
            }
            age_by_days(&application_owned, 60);
            age_by_days(&hub_stale, 45);

            let mut config = service_printing("svc", LONG_RUNNING, captured(EffectiveLogMode::Off));
            config.logging = EffectiveLogging {
                mode: EffectiveLogMode::Always,
                source: LogSource::External,
                external_path: Some(application_owned.display().to_string()),
            };
            let (core, _sink) = core_logging_to(&dir, config);

            let preview = core.cleanup_preview(Some("svc"));
            let swept = core.cleanup_logs(Some("svc"));

            assert_eq!(preview.removed, vec![hub_stale.clone()]);
            assert_eq!(swept.removed, preview.removed);
            assert!(!hub_stale.exists(), "the Hub's own stale log survived");
            assert!(
                application_owned.exists(),
                "an application-owned log was deleted"
            );
            assert!(
                hub_current.exists(),
                "the newest run's log was deleted by the age rule"
            );
        }

        /// A core with nowhere to write answers both retention questions with
        /// "nothing", rather than reaching for a root it does not have.
        #[test]
        fn a_rootless_core_previews_and_sweeps_nothing() {
            let (core, _sink) = core_with_command("svc", LONG_RUNNING);

            assert_eq!(core.cleanup_preview(Some("svc")).removed.len(), 0);
            assert_eq!(core.cleanup_logs(Some("svc")).removed.len(), 0);
        }

        /// Move a file's modification time into the past, the way retention
        /// measures age.
        fn age_by_days(path: &FsPath, days: u64) {
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("the file is openable for write")
                .set_modified(when)
                .expect("the timestamp is settable");
        }
    }

    /// The quick entry's refusals (`#62`), which need no shell to test: what
    /// they assert is that nothing is registered and nothing is announced when
    /// the machine or the entry cannot supply what a terminal needs.
    #[test]
    fn the_quick_entry_refuses_a_machine_with_no_powershell() {
        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink.clone());

        let error = core
            .create_temporary_terminal_with(None, &NoShells, Some(test_cwd()))
            .expect_err("without a shell there is no terminal to create");

        assert_eq!(error.operation, "create_terminal");
        assert!(error.message.contains(temporary::POWERSHELL_7), "{error:?}");
        assert!(
            error.message.contains(temporary::WINDOWS_POWERSHELL),
            "{error:?}"
        );
        assert!(
            core.snapshots().is_empty() && core.entries().is_empty(),
            "a terminal that could not be created was registered anyway"
        );
        assert!(
            !sink.names().contains(&"session-created"),
            "a terminal that never existed was announced: {:?}",
            sink.names()
        );
    }

    /// A directory that is not there is a failure about that directory — and
    /// the session it was going to be is not left half-registered.
    #[test]
    fn the_quick_entry_refuses_a_directory_that_is_not_there() {
        let sink = Arc::new(RecordingSink::default());
        let core = SessionCore::new(sink.clone());
        let missing = std::env::temp_dir().join("lch-t62-no-such-entry-directory");

        let error = core
            .create_temporary_terminal_with(
                Some(&missing.to_string_lossy()),
                &OneShell {
                    name: temporary::WINDOWS_POWERSHELL,
                    program: windows_powershell(),
                },
                Some(test_cwd()),
            )
            .expect_err("a directory that does not exist cannot host a terminal");

        assert!(error.message.contains("does not exist"), "{error:?}");
        assert!(core.snapshots().is_empty());
        assert!(!sink.names().contains(&"session-created"));
    }

    /// A configured session is removed by editing the config file, and the
    /// window is not a second way to delete it.
    #[test]
    fn a_configured_session_is_not_removable_from_the_window() {
        let (core, _sink) = core_with("svc");

        let error = core
            .remove_session("svc")
            .expect_err("a configured session belongs to the config file");

        assert_eq!(error.kind, SessionErrorKind::Failed);
        assert!(error.message.contains("config file"), "{error:?}");
        assert!(
            core.snapshot("svc").is_some(),
            "the refused removal must leave the session alone"
        );
    }

    #[test]
    fn removing_a_session_nothing_registered_is_refused() {
        let core = SessionCore::without_listener();

        let error = core
            .remove_session("ghost")
            .expect_err("there is nothing to remove");

        assert_eq!(error.kind, SessionErrorKind::UnknownSession);
        assert_eq!(error.operation, "remove_session");
    }

    /// The removal gate as a table (#62, with #61's ownership rule behind it).
    ///
    /// The row that matters is `Error` with a run: that is how a stop which
    /// could not confirm the tree was gone is reported (`RunEnding::Failed`),
    /// and such a session still owns the only handle accounting for a live
    /// process tree — dropping it to tidy a list is exactly what #62 forbids.
    /// `Error` without a run is the other `Error`: a start that never got one.
    #[test]
    fn only_a_settled_temporary_session_is_removable() {
        use crate::session::state::ALL_STATUSES;
        use SessionStatus::*;

        for status in ALL_STATUSES {
            for owns_run in [false, true] {
                let expected = matches!(status, Stopped | Exited) || (status == Error && !owns_run);
                assert_eq!(
                    removable(true, status, owns_run),
                    expected,
                    "temporary session in {}, owning a run: {owns_run}",
                    status.as_str()
                );
                assert!(
                    !removable(false, status, owns_run),
                    "a configured session is never removable ({})",
                    status.as_str()
                );
            }
        }
    }

    /// The terminal half of Session Core (T07 #8).
    ///
    /// These host a real PowerShell, like T02's own tests: the PTY contract was
    /// validated there, so what is under test here is what the *session* makes
    /// of it — which kind of run a terminal session starts, where its output
    /// goes, what the offsets an attached view reads actually promise, and what
    /// happens to the shell when the session is stopped.
    ///
    /// Windows-only: off Windows there is no ConPTY backend to host a shell
    /// (`pty::unsupported` answers every operation with `UnsupportedPlatform`),
    /// so every test in this module would fail there. The predicate is spelled
    /// exactly as the PTY suite's — `test` included, though the enclosing
    /// module is already `#[cfg(test)]` — so one grep finds every suite in the
    /// crate that needs Windows (#30).
    #[cfg(all(test, windows))]
    mod terminal_tests {
        use super::*;
        use crate::pty::INTERRUPT_BYTE;
        use base64::Engine;

        /// The absolute path T02's tests use, so these do not depend on how
        /// PATH happens to be set in the environment the tests run in.
        const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\PowerShell.exe";

        /// Cold PowerShell on a busy machine can take a while to render its
        /// first prompt.
        const STARTUP: std::time::Duration = std::time::Duration::from_secs(40);

        fn terminal(id: &str) -> SessionConfig {
            SessionConfig {
                id: id.to_owned(),
                name: format!("Terminal {id}"),
                session_type: SessionType::Terminal,
                cwd: Some(test_cwd()),
                command: None,
                url: None,
                port: None,
                purpose: None,
                close_impact: None,
                shell: Some(format!("{POWERSHELL} -NoLogo -NoProfile")),
                initial_command: None,
                display: DisplayMode::Internal,
                lifecycle: LifecycleOwner::Managed,
                // A terminal's default policy: nothing is persisted
                // (`docs/LOGGING.md` §1.2), while its scrollback is still kept.
                logging: EffectiveLogging {
                    mode: EffectiveLogMode::Off,
                    source: LogSource::None,
                    external_path: None,
                },
            }
        }

        fn core_with_terminal(
            id: &str,
            configure: impl FnOnce(&mut SessionConfig),
        ) -> (SessionCore, Arc<RecordingSink>) {
            let sink = Arc::new(RecordingSink::default());
            let core = SessionCore::new(sink.clone());
            let mut config = terminal(id);
            configure(&mut config);
            core.register(config).expect("registration succeeds");
            (core, sink)
        }

        /// Everything the session has taken from the shell so far.
        fn scrollback(core: &SessionCore, id: &str) -> String {
            core.terminal_buffer(id)
                .unwrap_or_default()
                .iter()
                .map(|chunk| chunk.text())
                .collect()
        }

        /// Poll until `check` holds, so a test asserts on what the terminal
        /// actually produced rather than on how long it took.
        fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
            let deadline = std::time::Instant::now() + STARTUP;
            while !check() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        /// Wait for `marker` to be rendered by the hosted shell.
        ///
        /// The marker strings in these tests are assembled inside the shell at
        /// run time, so only the executed command's output can satisfy the
        /// wait — the echoed command line cannot (`LCH-ECHO-` + `1A` never
        /// appears literally in what was sent).
        fn expect_in_scrollback(core: &SessionCore, id: &str, marker: &str) {
            let deadline = std::time::Instant::now() + STARTUP;
            while std::time::Instant::now() < deadline {
                if scrollback(core, id).contains(marker) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // The transcript, so a failure here says what the shell did
            // instead of only what was expected of it.
            panic!(
                "the terminal never showed `{marker}`, saw: {:?}",
                scrollback(core, id)
            );
        }

        /// Send one command line, the way a terminal sends Enter.
        fn send(core: &SessionCore, id: &str, line: &str) {
            core.terminal_write(id, format!("{line}\r").as_bytes())
                .expect("input reaches the terminal");
        }

        #[test]
        fn starting_a_terminal_hosts_a_shell_and_reports_it_attached() {
            let (core, _sink) = core_with_terminal("term", |_| {});

            let runtime = core.start("term").expect("start succeeds");

            assert_eq!(runtime.status, SessionStatus::Running);
            assert!(runtime.pid.is_some(), "a running terminal has a pid");
            assert!(runtime.run_id.is_some(), "a run has an id");
            assert!(
                runtime.pty_attached,
                "a terminal run reports the PTY the view types into"
            );
            assert_eq!(runtime.last_error, None);

            let attached = core
                .terminal_attachment("term")
                .expect("the session attaches");
            assert!(attached.pty_attached);
            assert!(attached.generation > 0, "the attachment names a run");

            core.stop("term").expect("cleanup");
        }

        /// The whole point of T07: what the shell prints reaches the session
        /// the UI renders, through the terminal the user is actually driving.
        #[test]
        fn a_command_in_the_hosted_shell_reaches_the_sessions_scrollback() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");

            send(&core, "term", "Write-Host (\"LCH-ECHO-\" + \"1A\")");

            expect_in_scrollback(&core, "term", "LCH-ECHO-1A");
            core.stop("term").expect("cleanup");
        }

        /// Spec §7: "For terminal sessions, Ctrl+C is not the same action as
        /// closing the session." The interrupt travels as input (T02's
        /// `INTERRUPT_BYTE`), so the session must still be running after it and
        /// must still accept the next command.
        #[test]
        fn ctrl_c_is_input_and_does_not_close_the_session() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            expect_in_scrollback(&core, "term", "PS");

            // Interrupt whatever is (or is about to be) running, then prove the
            // shell still answers.
            core.terminal_write("term", &[INTERRUPT_BYTE])
                .expect("the interrupt reaches the terminal");
            std::thread::sleep(std::time::Duration::from_millis(500));
            send(&core, "term", "Write-Host (\"LCH-AFTER-\" + \"CTRLC\")");

            expect_in_scrollback(&core, "term", "LCH-AFTER-CTRLC");
            let runtime = core.snapshot("term").expect("the session is registered");
            assert_eq!(
                runtime.status,
                SessionStatus::Running,
                "Ctrl+C must not close the session"
            );
            assert!(runtime.pty_attached);

            core.stop("term").expect("cleanup");
        }

        /// The other half of the Ctrl+C criterion: it has to actually
        /// *interrupt* what the shell is running (`MVP §16`, "Ctrl+C interrupts
        /// a long-running command") — at the session level, not just at the
        /// layer below.
        ///
        /// The running command sleeps for five minutes. The follow-up command
        /// can only answer within [`STARTUP`] if the interrupt reached the
        /// shell and gave the prompt back; without it the test times out rather
        /// than passing late.
        #[test]
        fn ctrl_c_interrupts_the_command_that_is_running() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            expect_in_scrollback(&core, "term", "PS");

            // The marker proves the pipeline is *executing* before the
            // interrupt arrives, rather than still being typed — the same
            // shape T02's own Ctrl+C test uses.
            send(
                &core,
                "term",
                "Write-Host (\"LCH-STARTED-\" + \"CTRLC\"); Start-Sleep -Seconds 300;                  Write-Host (\"LCH-NEVER-\" + \"CTRLC\")",
            );
            expect_in_scrollback(&core, "term", "LCH-STARTED-CTRLC");

            core.terminal_write("term", &[INTERRUPT_BYTE])
                .expect("the interrupt reaches the terminal");
            // Wait for the new prompt after the execution marker, not the
            // startup prompt already in scrollback. A fixed delay races the
            // console host's interrupt handling on busy CI runners, which
            // flushes pending input and can discard the follow-up command.
            wait_until("the prompt after Ctrl+C", || {
                scrollback(&core, "term")
                    .split_once("LCH-STARTED-CTRLC")
                    .is_some_and(|(_, after)| after.contains("PS ") && after.contains("> "))
            });
            send(&core, "term", "Write-Host (\"LCH-RESUMED-\" + \"CTRLC\")");

            expect_in_scrollback(&core, "term", "LCH-RESUMED-CTRLC");
            // The sleep was cancelled, so the marker after it never ran. Only
            // the executed command could print it: the echoed line splits the
            // literals, so the echo cannot satisfy this.
            assert!(
                !scrollback(&core, "term").contains("LCH-NEVER-CTRLC"),
                "the interrupted command's tail must not run"
            );
            assert_eq!(
                core.snapshot("term").expect("registered").status,
                SessionStatus::Running
            );

            core.stop("term").expect("cleanup");
        }

        /// Stopping a terminal closes the console, and the shell with it: the
        /// session is not stopped while a shell it accounted for keeps running.
        #[test]
        fn stopping_a_terminal_ends_the_run_and_closes_the_shell() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            let started = core.start("term").expect("start succeeds");
            let pid = started.pid.expect("a running terminal has a pid");

            let stopped = core.stop("term").expect("stop succeeds");

            assert_eq!(stopped.status, SessionStatus::Stopped);
            assert!(!stopped.pty_attached, "nothing is attached after a stop");
            assert_eq!(stopped.pid, None);
            wait_until("the hosted shell to be gone", || !pid_is_alive(pid));

            // The session is still there, and says why it cannot be typed into
            // rather than accepting bytes that would go nowhere.
            let error = core
                .terminal_write("term", b"echo hi\r")
                .expect_err("a stopped terminal takes no input");
            assert!(
                error.message.contains("no running terminal"),
                "unexpected message: {}",
                error.message
            );
        }

        /// A private directory for the batch-file handshake these tests use.
        ///
        /// The same shape `process`'s startup regression uses: the child
        /// publishes its own pid, so an assertion about the tree is about a
        /// process the test can name rather than about a name that might match
        /// anything (`docs/DEVELOPMENT.md` §6).
        struct TerminalFixtureDir(PathBuf);

        impl TerminalFixtureDir {
            fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};

                static SEQUENCE: AtomicU32 = AtomicU32::new(0);

                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("the clock is after the epoch")
                    .as_nanos();
                let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "lch-t61-{}-{unique}-{sequence}",
                    std::process::id()
                ));
                std::fs::create_dir(&path).expect("the fixture directory is created");
                TerminalFixtureDir(path)
            }

            fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for TerminalFixtureDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// A shell that starts a child of its own, plus the file that child
        /// publishes its pid through.
        ///
        /// `keep_shell` picks which half of the close is under test: a shell
        /// that stays alive is the close the user asks for, and one that waits
        /// for the handshake and then exits is the run ending while part of its
        /// tree is still running.
        fn shell_with_a_child(dir: &std::path::Path, keep_shell: bool) -> (String, PathBuf) {
            let ready = dir.join("child-ready.txt");
            // `start /b` gives the child the shell's console and, with it, the
            // shell's job: the descendant is a child in every sense the Hub
            // cares about, not a process that detached from the tree. It
            // publishes its own pid, then stays alive to be found.
            std::fs::write(
                dir.join("child.cmd"),
                "@echo off\r\nstart \"\" /b powershell.exe -NoProfile -Command \"Set-Content -LiteralPath '%~dp0child-ready.txt' -Value $PID; Start-Sleep -Seconds 300\"\r\n",
            )
            .expect("the child launcher script is written");

            let (name, script) = if keep_shell {
                (
                    "holder.cmd",
                    "@echo off\r\ncall \"%~dp0child.cmd\"\r\nping -n 60 127.0.0.1 > NUL\r\n",
                )
            } else {
                (
                    "launcher.cmd",
                    // The shell outlives the handshake, not the child: it exits
                    // the moment this test can name the process it left behind.
                    "@echo off\r\ncall \"%~dp0child.cmd\"\r\n:wait\r\nif not exist \"%~dp0child-ready.txt\" (ping -n 1 -w 200 127.0.0.1 > NUL & goto wait)\r\nexit /b 0\r\n",
                )
            };
            std::fs::write(dir.join(name), script).expect("the shell script is written");

            (
                format!("cmd.exe /d /c \"{}\"", dir.join(name).display()),
                ready,
            )
        }

        /// The descendant's pid, once it has published it.
        fn wait_for_handshake(ready: &std::path::Path) -> u32 {
            let deadline = std::time::Instant::now() + STARTUP;
            loop {
                if let Ok(text) = std::fs::read_to_string(ready) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        return pid;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the shell's child never published its pid"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        /// A process the Hub never started, with the same executable name the
        /// terminal's shell has.
        struct UnrelatedCmd(std::process::Child);

        impl UnrelatedCmd {
            fn start() -> Self {
                let child = std::process::Command::new("cmd.exe")
                    .args(["/c", "ping -n 60 127.0.0.1 > NUL"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("the unrelated process starts");
                UnrelatedCmd(child)
            }

            /// Whether it is still running. Asked of the process object rather
            /// than of a process list, so a pid that has been terminated is
            /// never reported as alive.
            fn is_running(&mut self) -> bool {
                self.0
                    .try_wait()
                    .expect("the process is waitable")
                    .is_none()
            }
        }

        impl Drop for UnrelatedCmd {
            fn drop(&mut self) {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                let _ = self.0.wait();
            }
        }

        /// Closing a terminal ends the shell *and* what the shell started.
        #[test]
        fn stopping_a_terminal_ends_the_processes_its_shell_started() {
            let dir = TerminalFixtureDir::new();
            let (shell, ready) = shell_with_a_child(dir.path(), true);
            let (core, _sink) = core_with_terminal("term", |config| {
                config.cwd = Some(dir.path().to_path_buf());
                config.shell = Some(shell);
            });

            core.start("term").expect("start succeeds");
            let descendant = wait_for_handshake(&ready);
            assert!(
                pid_is_alive(descendant),
                "the shell's child should be running before the close"
            );

            let stopped = core.stop("term").expect("stop succeeds");

            assert_eq!(stopped.status, SessionStatus::Stopped);
            wait_until("the shell's child to be gone", || !pid_is_alive(descendant));
        }

        /// A shell that exits first does not take the run with it: the session
        /// is not allowed to say it has ended while the tree it owned is still
        /// running (spec #59 decision 13).
        ///
        /// Nobody asks for this ending, so the assertion is on the *order* the
        /// watcher publishes in — the run's own process is gone well before the
        /// status stops saying `Running`, and the child is what has to be gone
        /// by then.
        #[test]
        fn a_shell_that_exits_first_still_ends_its_tree_before_the_session_ends() {
            let dir = TerminalFixtureDir::new();
            let (shell, ready) = shell_with_a_child(dir.path(), false);
            let (core, _sink) = core_with_terminal("term", |config| {
                config.cwd = Some(dir.path().to_path_buf());
                config.shell = Some(shell);
            });

            core.start("term").expect("start succeeds");
            let descendant = wait_for_handshake(&ready);

            wait_until("the terminal to end on its own", || {
                core.snapshot("term")
                    .expect("the session is registered")
                    .status
                    != SessionStatus::Running
            });
            assert!(
                !pid_is_alive(descendant),
                "the session reported an ending while the tree it owned was still running"
            );
        }

        /// A close ends one tree: not the sessions next to it, and not a
        /// process that merely shares the shell's name. Everything here is
        /// `cmd.exe`, which is the point — the Hub owns processes, not names
        /// (`docs/DEVELOPMENT.md` §6).
        #[test]
        fn closing_a_terminal_leaves_other_sessions_and_unrelated_processes_alone() {
            let dir = TerminalFixtureDir::new();
            let (shell, ready) = shell_with_a_child(dir.path(), true);
            let (core, _sink) = core_with_terminal("term", |config| {
                config.cwd = Some(dir.path().to_path_buf());
                config.shell = Some(shell);
            });
            core.register(terminal("other"))
                .expect("the neighbouring session registers");

            core.start("term").expect("start succeeds");
            let descendant = wait_for_handshake(&ready);
            let neighbour = core.start("other").expect("the neighbour starts");
            let neighbour_pid = neighbour.pid.expect("a running terminal has a pid");
            let mut unrelated = UnrelatedCmd::start();

            core.stop("term").expect("stop succeeds");

            assert!(
                !pid_is_alive(descendant),
                "the closed terminal's own child must still be gone"
            );
            assert!(
                unrelated.is_running(),
                "an unrelated cmd.exe must not be part of anyone's tree"
            );
            let neighbour = core.snapshot("other").expect("the neighbour is registered");
            assert_eq!(neighbour.status, SessionStatus::Running);
            assert_eq!(neighbour.pid, Some(neighbour_pid));

            core.stop("other").expect("cleanup");
        }

        #[test]
        fn restarting_a_terminal_replaces_the_run_and_leaves_one_shell() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            let first = core.start("term").expect("start succeeds");
            let first_pid = first.pid.expect("a running terminal has a pid");

            let second = core.restart("term").expect("restart succeeds");

            assert_eq!(second.status, SessionStatus::Running);
            assert_ne!(second.run_id, first.run_id, "a restart is a new run");
            assert_ne!(second.pid, first.pid, "a restart is a new shell");
            assert!(second.pty_attached);
            wait_until("the replaced shell to be gone", || !pid_is_alive(first_pid));

            // The view, the log answer, the snapshot and the summary are the
            // surfaces a restart must leave usable (#106): terminal, logs, details,
            // status bar.
            let attached = core
                .terminal_attachment("term")
                .expect("the terminal view still attaches after restart");
            assert!(attached.pty_attached);
            assert_eq!(attached.session_id, "term");
            assert!(core.terminal_buffer("term").is_some());
            core.terminal_write("term", b"\r")
                .expect("the restarted terminal still accepts input");
            let logs = core.log_status("term").expect("logs still answer");
            assert_eq!(logs.session_id, "term");
            let details = core.snapshot("term").expect("details still answer");
            assert_eq!(details.status, SessionStatus::Running);
            assert_eq!(details.pid, second.pid);
            assert_eq!(details.run_id, second.run_id);
            assert_eq!(core.summary().running, 1);

            core.stop("term").expect("cleanup");
        }

        /// A supervised service has no attached stdin in the MVP; saying so is
        /// the difference between a UI that explains itself and one that sends
        /// keystrokes into nothing.
        #[test]
        fn input_into_a_service_is_refused() {
            let (core, _sink) = core_with_command("svc", LONG_RUNNING);
            core.start("svc").expect("start succeeds");

            let error = core
                .terminal_write("svc", b"hello\r")
                .expect_err("a service takes no terminal input");
            assert_eq!(error.kind, SessionErrorKind::Unsupported);
            assert!(
                error.message.contains("supervised service"),
                "unexpected message: {}",
                error.message
            );

            core.force_stop("svc").expect("cleanup");
        }

        #[test]
        fn input_into_a_session_that_never_started_is_refused() {
            let (core, _sink) = core_with_terminal("term", |_| {});

            let error = core
                .terminal_write("term", b"echo hi\r")
                .expect_err("nothing is running");
            assert!(
                error.message.contains("no running terminal"),
                "unexpected message: {}",
                error.message
            );
        }

        /// An attachment is the whole handover: the scrollback so far, the
        /// generation it belongs to, and the offset it reaches. A view replays
        /// the chunks and appends what comes after the offset — which is only
        /// sound if the offset is where the *next* batch begins.
        #[test]
        fn every_batch_after_an_attachment_begins_where_the_attachment_ended() {
            let (core, sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            // Some output before the attach, so the scrollback is not empty and
            // the offset is not zero.
            send(&core, "term", "Write-Host (\"LCH-BEFORE-\" + \"ATTACH\")");
            expect_in_scrollback(&core, "term", "LCH-BEFORE-ATTACH");

            let attached = core
                .terminal_attachment("term")
                .expect("the session attaches");
            let after_attach = sink.events().len();
            assert!(attached.emitted > 0, "the replay covers what was printed");
            let replayed: usize = attached
                .chunks
                .iter()
                .map(|chunk| chunk_text(chunk).len())
                .sum();
            assert_eq!(
                replayed as u64, attached.emitted,
                "one run in, the offset is exactly what the replay covers"
            );

            send(&core, "term", "Write-Host (\"LCH-AFTER-\" + \"ATTACH\")");
            expect_in_scrollback(&core, "term", "LCH-AFTER-ATTACH");

            // The scrollback takes bytes before the batch that carries them is
            // published, so wait for the publication rather than assume it.
            let published = || -> Vec<TerminalOutput> {
                sink.events()
                    .into_iter()
                    .skip(after_attach)
                    .filter_map(|event| match event {
                        SessionEvent::TerminalOutput(output) => Some(output),
                        _ => None,
                    })
                    .collect()
            };
            wait_until("the later output to be published", || {
                published().iter().any(|batch| batch.end > attached.emitted)
            });

            let live: Vec<TerminalOutput> = published()
                .into_iter()
                .filter(|batch| batch.end > attached.emitted)
                .collect();
            for batch in &live {
                assert!(
                    batch.start >= attached.emitted,
                    "batch {}..{} straddles the attachment offset {}, so a view \
                     that replayed the scrollback would have to splice it",
                    batch.start,
                    batch.end,
                    attached.emitted
                );
                assert_eq!(
                    batch.generation, attached.generation,
                    "a batch from another run must not be appended"
                );
            }

            core.stop("term").expect("cleanup");
        }

        /// After a restart, the offsets start over while the scrollback does
        /// not.
        ///
        /// The scrollback belongs to the *session* (`docs/LOGGING.md` §8 keeps
        /// it across a restart), so a view attaching afterwards replays the
        /// previous run's bytes too — while `emitted` counts the new run from
        /// zero. The rule still holds, and this is the case that would break if
        /// the offset were read as "how much text the view has rendered": the
        /// new run's very first batch ends past the offset and must be
        /// appended, not dropped as already shown.
        #[test]
        fn an_attachment_after_a_restart_counts_the_new_run_from_zero() {
            let (core, sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            send(&core, "term", "Write-Host (\"LCH-FIRST-\" + \"RUN\")");
            expect_in_scrollback(&core, "term", "LCH-FIRST-RUN");

            core.restart("term").expect("restart succeeds");
            let attached = core
                .terminal_attachment("term")
                .expect("the session attaches");

            let replayed: String = attached.chunks.iter().map(chunk_text).collect();
            assert!(
                replayed.contains("LCH-FIRST-RUN"),
                "the session's scrollback survives a restart: {replayed:?}"
            );
            assert!(
                replayed.len() as u64 >= attached.emitted,
                "the replay covers earlier runs as well as this one"
            );

            let after = sink.events().len();
            send(&core, "term", "Write-Host (\"LCH-SECOND-\" + \"RUN\")");
            wait_until("the new run's output to be published", || {
                sink.events().iter().skip(after).any(|event| {
                    matches!(
                        event,
                        SessionEvent::TerminalOutput(output)
                            if output.generation == attached.generation
                                && output.end > attached.emitted
                    )
                })
            });
            expect_in_scrollback(&core, "term", "LCH-SECOND-RUN");

            core.stop("term").expect("cleanup");
        }

        /// A view measures itself before its session starts, and a shell
        /// started at the default 80×24 and corrected a moment later is not the
        /// same screen — so the size is remembered rather than dropped.
        #[test]
        fn a_resize_before_the_start_geometries_the_shell() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.terminal_resize("term", 120, 30)
                .expect("a stopped session accepts a size");

            core.start("term").expect("start succeeds");
            send(
                &core,
                "term",
                "Write-Host (\"LCH-GEO-\" + \
                 \"$($Host.UI.RawUI.WindowSize.Width)x$($Host.UI.RawUI.WindowSize.Height)\")",
            );

            expect_in_scrollback(&core, "term", "LCH-GEO-120x30");
            core.stop("term").expect("cleanup");
        }

        #[test]
        fn a_resize_of_a_live_terminal_reaches_the_shell() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            expect_in_scrollback(&core, "term", "PS");

            core.terminal_resize("term", 100, 25)
                .expect("the terminal resizes");
            // A keystroke racing the console host's resize reinitialization can
            // be dropped by conhost — the ConPTY quirk noted on #3, and the
            // reason a view debounces its resize forwarding. Let the resize
            // settle before typing.
            std::thread::sleep(std::time::Duration::from_millis(500));
            send(
                &core,
                "term",
                "Write-Host (\"LCH-GEO-\" + \
                 \"$($Host.UI.RawUI.WindowSize.Width)x$($Host.UI.RawUI.WindowSize.Height)\")",
            );

            expect_in_scrollback(&core, "term", "LCH-GEO-100x25");
            core.stop("term").expect("cleanup");
        }

        #[test]
        fn an_unhostable_size_is_refused() {
            let (core, _sink) = core_with_terminal("term", |_| {});

            for (cols, rows) in [(0, 24), (80, 0), (MAX_DIMENSION + 1, 24)] {
                let error = core
                    .terminal_resize("term", cols, rows)
                    .expect_err("the size is not hostable");
                assert!(
                    error.message.contains("cells"),
                    "unexpected message for {cols}x{rows}: {}",
                    error.message
                );
            }
        }

        #[test]
        fn an_initial_command_is_typed_into_the_terminal() {
            let (core, _sink) = core_with_terminal("term", |config| {
                config.initial_command = Some("Write-Host (\"LCH-INIT-\" + \"2B\")".to_owned());
            });

            core.start("term").expect("start succeeds");

            expect_in_scrollback(&core, "term", "LCH-INIT-2B");
            core.stop("term").expect("cleanup");
        }

        /// A view is not what keeps a terminal alive.
        ///
        /// Selecting another session, switching to the Logs tab or hiding the
        /// window all look the same from here: nobody is attached. The shell
        /// must keep running, keep producing output, and be there — with
        /// everything it printed — when a view comes back (spec §6: "terminal
        /// session remaining alive while hidden or while another session is
        /// selected").
        #[test]
        fn a_terminal_keeps_running_while_no_view_is_attached() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            let started = core.start("term").expect("start succeeds");
            let pid = started.pid.expect("a running terminal has a pid");

            send(&core, "term", "Write-Host (\"LCH-UNSEEN-\" + \"1\")");
            expect_in_scrollback(&core, "term", "LCH-UNSEEN-1");

            // Nothing reads the terminal from here on.
            std::thread::sleep(std::time::Duration::from_millis(300));
            send(&core, "term", "Write-Host (\"LCH-LATER-\" + \"2\")");
            expect_in_scrollback(&core, "term", "LCH-LATER-2");

            let runtime = core.snapshot("term").expect("the session is registered");
            assert_eq!(
                runtime.status,
                SessionStatus::Running,
                "an unwatched terminal must keep running"
            );
            assert_eq!(runtime.pid, Some(pid), "it is still the same shell");

            // A view arriving afterwards is given what it missed.
            let attached = core
                .terminal_attachment("term")
                .expect("the session attaches");
            assert!(attached.pty_attached);
            let replayed: String = attached.chunks.iter().map(chunk_text).collect();
            assert!(
                replayed.contains("LCH-UNSEEN-1") && replayed.contains("LCH-LATER-2"),
                "the replay must cover output from while nobody was watching: {replayed:?}"
            );

            core.stop("term").expect("cleanup");
        }

        /// The acceptance criterion behind the byte transport: a shell's output
        /// is UTF-8 plus ANSI, and it has to survive the trip through the
        /// session without being decoded as text on the way.
        ///
        /// The command prints the characters from their code points rather than
        /// sending them as input, so this measures the output path (what T07
        /// owns) and not the console's input code page.
        #[test]
        fn non_ascii_output_survives_the_session() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");

            send(
                &core,
                "term",
                "Write-Host (\"LCH-\" + [char]0x4E2D + [char]0x6587 + \"-\" + [char]0x2713)",
            );

            expect_in_scrollback(&core, "term", "LCH-中文-✓");
            core.stop("term").expect("cleanup");
        }

        /// Spec §6 requires Unicode **input** as well as output: a keystroke is
        /// a byte, and the session must carry it to the shell rather than
        /// mangling it on the way in.
        ///
        /// The command is sent as UTF-8 bytes through `terminal_write` and asks
        /// the shell to echo what it received as characters — so the assertion
        /// only holds if the bytes arrived intact. The characters are compared
        /// by code point (`[int][char]`) rather than by their shape, which
        /// keeps the test about the transport rather than about how this
        /// machine's console renders CJK text.
        #[test]
        fn non_ascii_input_reaches_the_shell() {
            let (core, _sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            expect_in_scrollback(&core, "term", "PS");

            // Typed as input, exactly as a paste would arrive.
            send(
                &core,
                "term",
                "$t = '中文✓'; $c = ($t.ToCharArray() | ForEach-Object { [int]$_ }) -join '-';                  Write-Host (\"LCH-IN-\" + $c)",
            );

            // 中 = 0x4E2D, 文 = 0x6587, ✓ = 0x2713.
            expect_in_scrollback(&core, "term", "LCH-IN-20013-25991-10003");
            core.stop("term").expect("cleanup");
        }

        /// Spec §14: "one high-output session must not freeze the whole UI",
        /// and §9: output must be "batched enough that high-volume output does
        /// not create an expensive UI event per line".
        ///
        /// The burst is 2 000 lines. What is asserted is not how fast it
        /// arrived but its *shape*: every batch is inside the size ceiling,
        /// batches are contiguous (no byte is skipped or repeated), and the
        /// whole burst is far fewer events than it is lines.
        #[test]
        fn a_burst_of_output_is_published_in_bounded_contiguous_batches() {
            let (core, sink) = core_with_terminal("term", |_| {});
            core.start("term").expect("start succeeds");
            expect_in_scrollback(&core, "term", "PS");
            let before = sink.events().len();

            send(
                &core,
                "term",
                "for ($i = 0; $i -lt 2000; $i++) { Write-Host \"LCH-LINE $i\" }; \
                 Write-Host (\"LCH-BURST-\" + \"END\")",
            );
            expect_in_scrollback(&core, "term", "LCH-BURST-END");

            let batches: Vec<TerminalOutput> = sink
                .events()
                .into_iter()
                .skip(before)
                .filter_map(|event| match event {
                    SessionEvent::TerminalOutput(output) => Some(output),
                    _ => None,
                })
                .collect();

            assert!(!batches.is_empty(), "the burst produced no output events");
            for pair in batches.windows(2) {
                assert_eq!(
                    pair[0].end, pair[1].start,
                    "batches must be contiguous: {}..{} then {}..{}",
                    pair[0].start, pair[0].end, pair[1].start, pair[1].end
                );
            }
            for batch in &batches {
                let covered = batch.end - batch.start;
                assert_eq!(
                    covered,
                    batch_bytes(batch) as u64,
                    "a batch's range must be exactly the bytes it carries"
                );
                assert!(
                    covered <= crate::session::terminal::BATCH_MAX_BYTES as u64,
                    "batch {}..{} is larger than the ceiling",
                    batch.start,
                    batch.end
                );
            }
            assert!(
                batches.len() < 200,
                "a 2 000-line burst must not become one event per line; got {} events",
                batches.len()
            );

            let text = scrollback(&core, "term");
            assert!(
                text.contains("LCH-LINE 0") && text.contains("LCH-LINE 1999"),
                "the whole burst must reach the scrollback"
            );
            core.stop("term").expect("cleanup");
        }

        /// One retained chunk as text, for assertions about a replay.
        fn chunk_text(chunk: &RetainedChunk) -> String {
            String::from_utf8_lossy(
                &base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data)
                    .expect("chunks arrive as base64"),
            )
            .into_owned()
        }

        /// The bytes one published batch carries.
        fn batch_bytes(batch: &TerminalOutput) -> usize {
            base64::engine::general_purpose::STANDARD
                .decode(&batch.data)
                .expect("batches arrive as base64")
                .len()
        }
    }

    /// The quick entry's half of Session Core (#62).
    ///
    /// Windows-only, for the same reason `terminal_tests` is: these create real
    /// PowerShell sessions on a ConPTY, and off Windows there is no backend to
    /// host one (`pty::unsupported`). Their fixtures are deliberately local
    /// rather than borrowed from `terminal_tests` — the two suites ask
    /// different questions, and a shared fixture would have to answer both.
    #[cfg(all(test, windows))]
    mod temporary_tests {
        use super::*;
        use crate::config::AppPaths;
        use crate::session::temporary;

        /// Cold PowerShell on a busy machine takes a while to render.
        const STARTUP: std::time::Duration = std::time::Duration::from_secs(40);

        /// The shell every one of these creates, and the one machine fact
        /// they are allowed to depend on: Windows PowerShell ships with
        /// Windows, so nothing here needs PowerShell 7 to be installed.
        fn machine() -> OneShell {
            OneShell {
                name: temporary::WINDOWS_POWERSHELL,
                program: windows_powershell(),
            }
        }

        fn core_with_sink() -> (SessionCore, Arc<RecordingSink>) {
            let sink = Arc::new(RecordingSink::default());
            (SessionCore::new(sink.clone()), sink)
        }

        /// Everything the session has taken from the shell so far.
        fn scrollback(core: &SessionCore, id: &str) -> String {
            core.terminal_buffer(id)
                .unwrap_or_default()
                .iter()
                .map(|chunk| chunk.text())
                .collect()
        }

        /// Send one command line, the way a terminal sends Enter.
        fn send(core: &SessionCore, id: &str, line: &str) {
            core.terminal_write(id, format!("{line}\r").as_bytes())
                .expect("input reaches the terminal");
        }

        /// Poll until `check` holds, so a test asserts on what happened rather
        /// than on how long it took.
        fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
            let deadline = std::time::Instant::now() + STARTUP;
            while !check() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        /// Every event the sink was told about `session_id`.
        fn about(sink: &RecordingSink, session_id: &str) -> Vec<SessionEvent> {
            sink.events()
                .into_iter()
                .filter(|event| event.session_id() == Some(session_id))
                .collect()
        }

        /// One click of "新建 PowerShell": a real shell, running, in the
        /// directory the entry names, with a registry row that says it is
        /// temporary (#62, story 6).
        #[test]
        fn one_click_creates_a_running_terminal_in_the_entrys_directory() {
            let (core, _sink) = core_with_sink();
            let directory = test_cwd();

            let created = core
                .create_temporary_terminal_with(
                    Some(&directory.to_string_lossy()),
                    &machine(),
                    Some(std::env::temp_dir()),
                )
                .expect("the entry creates a terminal");

            assert_eq!(created.config.session_type, SessionType::Terminal);
            assert_eq!(
                created.config.cwd.as_ref(),
                Some(&directory),
                "the directory the entry named is the one the shell opened in"
            );
            assert_eq!(
                created.config.shell.as_deref(),
                Some(temporary::shell_command(&windows_powershell()).as_str()),
                "the session runs the shell the machine resolved"
            );
            // A temporary shell persists nothing (decision 16, H07).
            assert_eq!(created.config.logging.mode, EffectiveLogMode::Off);
            assert_eq!(created.config.logging.source, LogSource::None);

            assert_eq!(
                created.runtime.status,
                SessionStatus::Running,
                "the entry starts the terminal, it does not file it as stopped"
            );
            assert!(created.runtime.pty_attached, "a real shell is attached");

            // The registry and the answer describe the same session.
            let entry = core
                .entries()
                .into_iter()
                .find(|entry| entry.config.id == created.config.id)
                .expect("the created session is in the registry");
            assert!(
                entry.temporary,
                "the row is removable because it is temporary"
            );
            assert_eq!(core.configs().len(), 1);

            core.stop(&created.config.id).expect("cleanup");
        }

        /// The window is told about the session *before* it is told anything
        /// about its state, which is what lets a listener render the row a
        /// state event is about (§9, decision 14).
        #[test]
        fn a_creation_is_announced_before_any_of_its_states() {
            let (core, sink) = core_with_sink();

            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");

            let events = about(&sink, &created.config.id);
            match events.first() {
                Some(SessionEvent::Created(payload)) => {
                    assert_eq!(payload.session_id, created.config.id);
                    assert_eq!(payload.config.session_type, "terminal");
                    assert!(
                        payload.config.temporary,
                        "the announcement says the session is temporary"
                    );
                }
                other => {
                    panic!("the first event about a new session must be its creation: {other:?}")
                }
            }
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, SessionEvent::StateChanged(state)
                        if state.runtime.status == SessionStatus::Running)),
                "the states that follow belong to a session the listener already knows"
            );

            core.stop(&created.config.id).expect("cleanup");
        }

        /// Story 9: each click is its own terminal — its own id, its own name,
        /// its own process.
        #[test]
        fn every_click_creates_a_separate_terminal() {
            let (core, _sink) = core_with_sink();

            let first = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the first terminal is created");
            let second = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the second terminal is created");

            assert_ne!(first.config.id, second.config.id);
            assert_ne!(
                first.config.name, second.config.name,
                "two rows with the same name would be indistinguishable"
            );
            assert_eq!(core.snapshots().len(), 2);
            assert_eq!(
                core.snapshot(&second.config.id)
                    .map(|runtime| runtime.status),
                Some(SessionStatus::Running)
            );

            assert_eq!(first.config.name, "PowerShell 1");
            assert_eq!(second.config.name, "PowerShell 2");
            core.stop(&first.config.id).expect("cleanup");
            core.remove_session(&first.config.id).expect("close first");
            let replacement = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("a replacement terminal opens");
            assert_eq!(
                replacement.config.name, "PowerShell 1",
                "reuse the vacant number"
            );
            assert_ne!(
                replacement.config.id, first.config.id,
                "a reused name is a new session"
            );
            assert_eq!(
                core.session_entry(&second.config.id).unwrap().config.name,
                "PowerShell 2"
            );
            core.stop(&second.config.id).expect("cleanup");
            core.remove_session(&second.config.id)
                .expect("close second");
            core.stop(&replacement.config.id).expect("cleanup");
            core.remove_session(&replacement.config.id)
                .expect("close replacement");
            let fresh = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("a fresh terminal opens");
            assert_eq!(
                fresh.config.name, "PowerShell 1",
                "an empty list starts at one"
            );
            core.stop(&fresh.config.id).expect("cleanup");
        }

        /// A shell that cannot start leaves no row behind — the creation is
        /// withdrawn, and the command answers with the failure (decision 4).
        #[test]
        fn a_terminal_that_cannot_start_leaves_no_row() {
            let (core, sink) = core_with_sink();
            // A real file that is not a program: it resolves, and the spawn
            // fails on it — the one failure a pre-flight check cannot catch.
            let impostor = std::env::temp_dir().join("lch-t62-not-a-program.txt");
            std::fs::write(&impostor, b"not a program").expect("the fixture file is writable");

            let error = core
                .create_temporary_terminal_with(
                    None,
                    &OneShell {
                        name: temporary::WINDOWS_POWERSHELL,
                        program: impostor.clone(),
                    },
                    Some(test_cwd()),
                )
                .expect_err("a file cannot host a terminal");

            std::fs::remove_file(&impostor).ok();
            assert_eq!(error.operation, "start");
            assert!(
                core.snapshots().is_empty() && core.entries().is_empty(),
                "a terminal that could not start left a row: {error:?}"
            );
            let names = sink.names();
            assert!(
                names.contains(&"session-created") && names.contains(&"session-removed"),
                "the withdrawn creation must be announced as withdrawn: {names:?}"
            );
        }

        /// Removing is for ended terminals: a live one owns a process tree.
        #[test]
        fn a_running_terminal_cannot_be_removed() {
            let (core, _sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");

            let error = core
                .remove_session(&created.config.id)
                .expect_err("a running terminal is not removable");

            assert!(error.message.contains("stop it"), "{error:?}");
            assert!(core.snapshot(&created.config.id).is_some());

            core.stop(&created.config.id).expect("cleanup");
        }

        /// Stories 21–22: an ended terminal keeps the output of the run the
        /// user just watched, stays in the list, and can then be removed —
        /// after which nothing can bring it back (decision 14).
        #[test]
        fn an_ended_terminal_keeps_its_output_until_it_is_removed() {
            let (core, sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();

            // The marker is assembled inside the shell, so only executed
            // output can satisfy the wait (T07's rule).
            send(&core, &id, "Write-Host (\"LCH-T62-\" + \"OK\")");
            wait_until("the shell to print the marker", || {
                scrollback(&core, &id).contains("LCH-T62-OK")
            });

            send(&core, &id, "exit");
            wait_until("the shell to end", || {
                core.snapshot(&id).map(|runtime| runtime.status) == Some(SessionStatus::Exited)
            });

            assert!(
                scrollback(&core, &id).contains("LCH-T62-OK"),
                "an ended terminal keeps the output of its run"
            );
            assert!(
                core.snapshot(&id).is_some(),
                "an ended terminal stays in the list until it is removed"
            );

            core.remove_session(&id)
                .expect("an ended terminal is removable");

            assert!(core.snapshot(&id).is_none());
            assert!(core.entries().iter().all(|entry| entry.config.id != id));
            let events = about(&sink, &id);
            assert!(
                matches!(events.last(), Some(SessionEvent::Removed(_))),
                "the removal is the last thing said about a removed session: {events:?}"
            );

            // And nothing published later can revive it: the publication path
            // itself finds no session, so a late watcher, log or output flush
            // cannot put the row back.
            let published = sink.events().len();
            assert!(
                !core.publish_state(&id),
                "a removed session must not be publishable again"
            );
            assert_eq!(
                sink.events().len(),
                published,
                "a late publication for a removed session reached a listener"
            );
        }

        /// H07/H08: a temporary shell's output goes to the scrollback and
        /// nowhere else, and the entry writes no config.
        ///
        /// Run *metadata* is deliberately not on that list: it is not output
        /// (`docs/LOGGING.md` §6 — "「这次运行发生过」本身就是记录"), every
        /// managed run has one, and a temporary terminal's record is what lets
        /// the Logs tab answer "what has this session run?" for the terminal
        /// the user is still looking at. So the assertions below are the two
        /// that matter: nothing captured the bytes, and no config entry was
        /// written.
        #[test]
        fn a_temporary_terminal_persists_no_output_and_writes_no_config() {
            let scratch = crate::logging::test_support::TempDir::new();
            let paths = AppPaths::new(scratch.path(), scratch.path());
            let sink = Arc::new(RecordingSink::default());
            let core = SessionCore::new(sink).with_log_roots(LogRoots::from_app_paths(&paths));

            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            send(&core, &id, "Write-Host (\"LCH-T62-\" + \"LOG\")");
            wait_until("the shell to print the marker", || {
                scrollback(&core, &id).contains("LCH-T62-LOG")
            });
            core.stop(&id).expect("cleanup");

            assert!(
                !paths.logs_dir.exists(),
                "a temporary terminal persisted output: {} exists",
                paths.logs_dir.display()
            );
            assert!(
                !paths.config_file.exists(),
                "the quick entry wrote to the config file: {} exists",
                paths.config_file.display()
            );
            let history = core.run_history(&id);
            assert_eq!(history.runs.len(), 1, "the run is recorded");
            assert!(
                history.runs[0].run.log_file.is_none() && !history.runs[0].log_is_openable(),
                "a temporary run's record must not claim a log file: {:?}",
                history.runs[0].run
            );
            assert!(
                scrollback(&core, &id).contains("LCH-T62-LOG"),
                "the output is still where a temporary terminal keeps it"
            );
        }

        /// The saved configuration of `created`, under a name the user chose.
        fn saved_config(created: &CreatedSession, name: &str) -> SessionConfig {
            let mut config = created.config.clone();
            config.name = name.to_owned();
            config
        }

        /// Stories 24–25, H08: saving a terminal states where its launch
        /// configuration lives and touches nothing that is running.
        #[test]
        fn saving_a_running_terminal_keeps_its_run_its_process_and_its_output() {
            let (core, sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            send(&core, &id, "Write-Host (\"LCH-T65-\" + \"KEPT\")");
            wait_until("the shell to print the marker", || {
                scrollback(&core, &id).contains("LCH-T65-KEPT")
            });
            let before = core.snapshot(&id).expect("the terminal is running");
            let name_before = created.config.name.clone();

            let entry = core
                .mark_saved(&id, saved_config(&created, "项目终端"))
                .expect("a temporary terminal can be saved");

            assert!(!entry.temporary, "a saved session is not removable");
            assert_eq!(entry.config.name, "项目终端");
            assert_eq!(entry.config.id, id, "the session keeps its identity");

            let after = core.snapshot(&id).expect("the terminal is still there");
            assert_eq!(
                after.run_id, before.run_id,
                "saving must not start, restart or copy the run"
            );
            assert_eq!(
                after.pid, before.pid,
                "the process is the one that was running"
            );
            assert_eq!(after.status, SessionStatus::Running);
            assert_eq!(core.snapshots().len(), 1, "one row, one process");
            assert!(
                scrollback(&core, &id).contains("LCH-T65-KEPT"),
                "the output of the run survived the save"
            );
            assert_eq!(
                core.configs()
                    .into_iter()
                    .find(|config| config.id == id)
                    .map(|config| config.name),
                Some("项目终端".to_owned()),
                "the listing is the saved configuration"
            );
            assert_ne!(name_before, "项目终端", "the fixture renamed something");

            // Announcing it is the caller's, and it happens only once the file
            // holds the entry — `app::terminals` publishes it there, and its
            // own tests pin that a refused save announces nothing.
            core.publish_saved(&id);
            let saved_events: Vec<SessionEvent> = about(&sink, &id)
                .into_iter()
                .filter(|event| matches!(event, SessionEvent::Saved(_)))
                .collect();
            match saved_events.as_slice() {
                [SessionEvent::Saved(payload)] => {
                    assert_eq!(payload.session_id, id);
                    assert_eq!(payload.config.name, "项目终端");
                    assert!(
                        !payload.config.temporary,
                        "a row the window still files as temporary is a row it will let the user \
                         remove"
                    );
                }
                other => panic!("exactly one save is announced, got {other:?}"),
            }

            core.stop(&id).expect("cleanup");
        }

        /// The consequence of being saved: the file owns the row now, so the
        /// window no longer offers to remove it — the same refusal a
        /// configured session has always had.
        #[test]
        fn a_saved_terminal_is_no_longer_the_windows_to_remove() {
            let (core, _sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            core.mark_saved(&id, saved_config(&created, "项目终端"))
                .expect("the terminal is saved");
            send(&core, &id, "exit");
            wait_until("the shell to end", || {
                core.snapshot(&id).map(|runtime| runtime.status) == Some(SessionStatus::Exited)
            });

            let error = core
                .remove_session(&id)
                .expect_err("a saved session belongs to the config file");

            assert!(error.message.contains("config file"), "{error:?}");
            assert!(
                core.snapshot(&id).is_some(),
                "the row survives the refused removal"
            );
        }

        /// A second save has nothing to say: the file already describes this
        /// session, and re-stating it would rewrite the name the user chose
        /// there.
        #[test]
        fn saving_an_already_saved_terminal_is_refused() {
            let (core, _sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            core.mark_saved(&id, saved_config(&created, "项目终端"))
                .expect("the first save");

            let error = core
                .mark_saved(&id, saved_config(&created, "另一个名字"))
                .expect_err("a saved session is not saved again");

            assert!(error.message.contains("already"), "{error:?}");
            assert_eq!(
                core.configs()
                    .into_iter()
                    .find(|config| config.id == id)
                    .map(|config| config.name),
                Some("项目终端".to_owned()),
                "the refused save must not have renamed anything"
            );

            core.stop(&id).expect("cleanup");
        }

        /// The configuration a save records has to be *this* session's: a
        /// registry whose key and whose configuration disagree would render
        /// one row under another session's name.
        #[test]
        fn a_configuration_naming_another_session_is_refused() {
            let (core, _sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            let mut elsewhere = created.config.clone();
            elsewhere.id = "somewhere-else".to_owned();

            let error = core
                .mark_saved(&id, elsewhere)
                .expect_err("a save describes the session it was made for");

            assert!(error.message.contains("somewhere-else"), "{error:?}");
            assert!(
                core.entries()
                    .iter()
                    .find(|entry| entry.config.id == id)
                    .is_some_and(|entry| entry.temporary),
                "the refused save must leave the session temporary"
            );

            core.stop(&id).expect("cleanup");
        }

        /// A session the registry does not hold cannot be saved, and the
        /// refusal says so rather than failing silently.
        #[test]
        fn saving_an_unknown_session_is_refused() {
            let (core, _sink) = core_with_sink();
            let created = core
                .create_temporary_terminal_with(None, &machine(), Some(test_cwd()))
                .expect("the entry creates a terminal");
            let id = created.config.id.clone();
            let config = created.config.clone();
            core.stop(&id).expect("cleanup");
            core.remove_session(&id)
                .expect("an ended terminal is removable");

            let error = core
                .mark_saved(&id, config)
                .expect_err("a removed session cannot be saved");

            assert_eq!(error.kind, SessionErrorKind::UnknownSession);
        }
    }

    /// Associating an application the user was already running (#67).
    ///
    /// The instance is a real process this test starts itself — the Hub must
    /// not have started it, which is the whole point — so these are the same
    /// kind of test as T03's: real pids, real process objects, real endings.
    mod external_tests {
        use super::*;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        /// A process outside the Hub, with the identity the Hub would have read
        /// from the process table.
        struct Outsider {
            child: KillOnDrop,
            identity: ProcessIdentity,
        }

        impl Outsider {
            fn start() -> Self {
                let child = Command::new("cmd.exe")
                    .args(["/c", "ping -n 120 127.0.0.1"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the outside process starts");
                let identity = ProcessIdentity::of_child(&child);
                Outsider {
                    child: KillOnDrop(child),
                    identity,
                }
            }

            fn pid(&self) -> u32 {
                self.child.0.id()
            }

            /// The handle the Hub would open on it, which is also what verifies
            /// the identity against the process object.
            fn open(&self) -> ExternalProcess {
                ExternalProcess::open(self.identity).expect("the outside process opens")
            }

            /// End it, and wait until the OS agrees it is gone.
            fn end(&mut self) {
                let _ = self.child.0.kill();
                let _ = self.child.0.wait();
            }

            /// Whether it is still running — asked of the test's *own* handle
            /// rather than through the Hub's, so "the instance survives a
            /// refused stop" is a fact about the process and not about the
            /// Hub's opinion of it.
            fn is_alive(&mut self) -> bool {
                self.child
                    .0
                    .try_wait()
                    .expect("the fixture process is waitable")
                    .is_none()
            }
        }

        /// A process the test started, ended when the test ends however it ends.
        struct KillOnDrop(Child);

        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        /// A long-running command that writes nothing, because a
        /// standalone-window run keeps the Hub's console rather than a pipe
        /// and a chatty fixture would bury the test output.
        const QUIET_LONG_RUNNING: &str = "cmd.exe /c ping -n 120 127.0.0.1 > NUL";

        /// A window entry: the shape #67 applies to.
        ///
        /// Deliberately with management *on*, which is the case where the
        /// association is the only thing standing between a stop and the
        /// user's application. A default (independent) entry would be refused
        /// by its configuration as well, and the tests below would then pass
        /// for the wrong reason.
        fn window_entry(id: &str) -> SessionConfig {
            let mut config = service(id);
            config.command = Some(QUIET_LONG_RUNNING.to_owned());
            config.display = DisplayMode::Window;
            config.lifecycle = LifecycleOwner::Managed;
            config
        }

        fn core_with_window_entry(id: &str) -> (SessionCore, Arc<RecordingSink>) {
            let sink = Arc::new(RecordingSink::default());
            let core = SessionCore::new(sink.clone());
            core.register(window_entry(id))
                .expect("registration succeeds");
            (core, sink)
        }

        /// Wait for a condition the watcher thread produces, so the assertion
        /// is about the state rather than about a sleep.
        fn wait_until(what: &str, condition: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !condition() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// The core promise: the session reports the application as running,
        /// with the outside process's own pid — and nothing was started.
        #[test]
        fn an_adopted_instance_makes_the_session_running_without_starting_anything() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let outsider = Outsider::start();

            let runtime = core
                .adopt("comfyui", outsider.identity, outsider.open())
                .expect("adoption succeeds");

            assert_eq!(runtime.status, SessionStatus::Running);
            assert_eq!(runtime.pid, Some(outsider.pid()));
            assert!(runtime.external, "the Hub did not start this run");
            assert_eq!(
                runtime.run_id, None,
                "a run the Hub did not start has no run id"
            );
            assert!(
                runtime.started_at.is_some(),
                "the process's own creation time is its start time"
            );
            assert_eq!(core.summary().running, 1);
            assert!(core.is_external("comfyui"));
        }

        /// Decision 11's 关联不自动取得终止权限: the Hub reports the instance and
        /// may bring its window forward, and that is all it may do with it.
        #[test]
        fn an_adopted_instance_cannot_be_stopped_restarted_or_force_stopped() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let mut outsider = Outsider::start();
            core.adopt("comfyui", outsider.identity, outsider.open())
                .expect("adoption succeeds");

            for error in [
                core.stop("comfyui").expect_err("a stop is refused"),
                core.force_stop("comfyui")
                    .expect_err("a force stop is refused"),
                core.restart("comfyui").expect_err("a restart is refused"),
            ] {
                assert_eq!(error.kind, SessionErrorKind::Unsupported, "{error:?}");
                assert!(
                    error.message.contains("did not start it"),
                    "the refusal has to say why: {}",
                    error.message
                );
            }

            assert!(
                outsider.is_alive(),
                "a refused lifecycle action must leave the instance running"
            );
            assert_eq!(
                core.snapshot("comfyui").expect("registered").status,
                SessionStatus::Running,
                "a refused stop must not move the session"
            );
        }

        /// Opening an application that is already adopted answers with the
        /// instance the Hub holds — it does not look outside again, and it
        /// certainly does not start a second copy (story 36).
        #[test]
        fn activating_an_adopted_session_answers_with_the_instance_it_holds() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let outsider = Outsider::start();
            core.adopt("comfyui", outsider.identity, outsider.open())
                .expect("adoption succeeds");

            let activation = core.activate("comfyui").expect("activation succeeds");

            assert!(!activation.started, "nothing was created by this call");
            assert_eq!(activation.runtime.pid, Some(outsider.pid()));
            assert_eq!(activation.runtime.status, SessionStatus::Running);
        }

        /// An instance that ends on its own is noticed, and the session stops
        /// claiming a run: the row must not say "running" for an application
        /// that has been closed (user story 48's honesty, applied backwards).
        #[test]
        fn an_adopted_instance_that_ends_returns_the_session_to_exited() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let mut outsider = Outsider::start();
            core.adopt("comfyui", outsider.identity, outsider.open())
                .expect("adoption succeeds");

            outsider.end();

            wait_until("the ending to be noticed", || {
                core.snapshot("comfyui")
                    .is_some_and(|runtime| runtime.status == SessionStatus::Exited)
            });
            let runtime = core.snapshot("comfyui").expect("registered");
            assert!(!runtime.external);
            assert_eq!(runtime.pid, None, "nothing is running under this entry");
            assert_eq!(core.summary().running, 0);
            assert!(
                !outsider.is_alive(),
                "the fixture really did end, so the ending noticed is a real one"
            );
        }

        /// The "明确新开" half: the Hub lets go of the instance and the entry is
        /// startable again — and the process it was associated with is left
        /// exactly as it was.
        #[test]
        fn releasing_an_association_leaves_the_instance_running() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let mut outsider = Outsider::start();
            core.adopt("comfyui", outsider.identity, outsider.open())
                .expect("adoption succeeds");

            let runtime = core.release_adopted("comfyui").expect("release succeeds");

            assert_eq!(runtime.status, SessionStatus::Stopped);
            assert!(!runtime.external);
            assert_eq!(runtime.pid, None);
            assert!(!core.is_external("comfyui"));
            assert!(
                outsider.is_alive(),
                "letting go of an instance is not ending it"
            );
        }

        /// A number Windows has reused is not the process the Hub identified,
        /// and the association is refused rather than made about somebody else
        /// (decision 11's PID-reuse rule).
        #[test]
        fn adopting_an_identity_that_does_not_match_is_refused() {
            let (core, _sink) = core_with_window_entry("comfyui");
            let outsider = Outsider::start();
            let stale = ProcessIdentity::for_test(
                outsider.identity.pid(),
                outsider.identity.created_at().wrapping_add(1),
            );

            let error = core
                .adopt("comfyui", stale, outsider.open())
                .expect_err("a stale identity is refused");

            assert_eq!(error.kind, SessionErrorKind::Failed, "{error:?}");
            assert_eq!(
                core.snapshot("comfyui").expect("registered").status,
                SessionStatus::Stopped,
                "a refused adoption must leave the session alone"
            );
        }

        /// A session with a run of the Hub's own is not one an outside instance
        /// may be adopted into: it already has its answer.
        #[test]
        fn adopting_into_a_running_session_is_refused() {
            let (core, _sink) = core_with_window_entry("comfyui");
            core.start("comfyui").expect("the Hub starts its own copy");
            let outsider = Outsider::start();

            let error = core
                .adopt("comfyui", outsider.identity, outsider.open())
                .expect_err("a running session cannot adopt");

            assert_eq!(error.kind, SessionErrorKind::InvalidTransition, "{error:?}");
            let runtime = core.snapshot("comfyui").expect("registered");
            assert_eq!(runtime.status, SessionStatus::Running);
            assert!(!runtime.external, "the run is still the Hub's own");

            core.force_stop("comfyui").expect("cleanup");
        }

        /// Releasing is idempotent: a session nothing is associated with is
        /// already where the caller wants it.
        #[test]
        fn releasing_a_session_with_no_association_changes_nothing() {
            let (core, _sink) = core_with_window_entry("comfyui");

            let runtime = core.release_adopted("comfyui").expect("release succeeds");

            assert_eq!(runtime.status, SessionStatus::Stopped);
            assert!(!runtime.external);
        }
    }
}
