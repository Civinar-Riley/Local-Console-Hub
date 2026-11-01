import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isFormErrorDto,
  type ConfigReportDto,
  type FormSaveOutcome,
  type NewApplicationFormDto,
  type SaveTerminalFormDto,
} from "../types/config";
import {
  SESSION_CREATED,
  SESSION_REMOVED,
  SESSION_SAVED,
  SESSION_STATE_CHANGED,
  isActivationOutcomeDto,
  isCreatedSessionDto,
  isSessionRuntimeDto,
  sessionErrorMessage,
  type ActivationOutcomeDto,
  type OpenResolutionDto,
} from "../types/runtime";
import { FIXTURE_SESSIONS } from "../state/fixtures";
import type { SessionView } from "../state/session-view";
import type { BackendConnection } from "../state/backend-connection";
import {
  watchSessionRegistry,
  type SessionRegistryController,
  type SessionRegistrySnapshot,
} from "../state/session-registry";
import {
  installVerificationReload,
  readVerificationSnapshot,
} from "./session-initialization-verification";

/** The workspace the shell renders, and the actions its controls call. */
export interface SessionRegistry {
  /** The sessions to render, in the registry's order. */
  sessions: SessionView[];
  /** The list source, including the period before the first live snapshot. */
  source: SessionSource;
  /** The backend is connected, but its session list has not loaded yet. */
  loading: boolean;
  /** Whether a backend is answering; this is separate from session lifecycle. */
  live: boolean;
  /** The most recent failed action, for the status bar. */
  error: string | null;
  /**
   * When `error` was reported, as `Date.now()` at the moment of the failure.
   * The status bar composes this against its own notices by recency, so a
   * success message the user earns after a refused command is not swallowed
   * by the older failure's text.
   */
  errorAt: number;
  /** A failed session initialization; the registry retries with capped delay. */
  initializationError: string | null;
  /** Startup config report, including file-level and per-entry problems. */
  configReport: ConfigReportDto | null;
  /** Failure to retrieve the report IPC payload itself. */
  configReportError: string | null;
  /**
   * Open a session (#64, spec #59 decision 10): start it if nothing is
   * running, and answer with the run it already has if something is.
   *
   * The one open operation, so a second click on a running application cannot
   * create a second one and a click while it is stopping is refused rather
   * than queued behind the barrier. Restarting is deliberately a different
   * control.
   *
   * Answers with the operation's own result — including what happened to an
   * application's *own* window (#66) — or `null` when the call failed, whose
   * message lands in `error`. The caller decides what to say about a window
   * that did not come forward; the registry is transport.
   */
  activate(sessionId: string): Promise<ActivationOutcomeDto | null>;
  /**
   * Answer the question `activate` asked (#67).
   *
   * Two answers and no others: associate the instance the user picked, or
   * start the Hub's own copy. Cancelling calls nothing, which is also what
   * happens — the dialog closing is the whole of "leave it as it is".
   *
   * The answer is the same shape `activate` gives, so the window reads one
   * thing either way — including the window step that follows an association.
   */
  resolveOpen(
    sessionId: string,
    resolution: OpenResolutionDto,
  ): Promise<ActivationOutcomeDto | null>;
  stop(sessionId: string): void;
  restart(sessionId: string): void;
  forceStop(sessionId: string): void;
  /**
   * Hand this session's configured URL or working directory to the OS (T08
   * #9). A session id, never a URL or path, keeps resolution inside Session
   * Core (`docs/DECISIONS.md` D-021).
   */
  openUrl(sessionId: string): void;
  openDirectory(sessionId: string): void;
  /**
   * Create a temporary terminal (#62) — the "新建 PowerShell" entry.
   *
   * Answers with the new session's id once the backend has created, started
   * and announced it, or `null` when it could not: the failure's message
   * lands in `error`, which the status bar shows, because a shell that is not
   * on the machine is the user's to fix (H06).
   */
  createTerminal(cwd?: string): Promise<string | null>;
  /**
   * Save a new application and add it to the list (#64) — "添加应用".
   *
   * Answers with the new session's id once the backend has validated,
   * registered *and* written it, or with the reason it was refused. The two
   * halves matter: a refusal that came from the config layer names the form
   * field it belongs to, and the dialog keeps the user's input so they can fix
   * that one box rather than retype the form.
   */
  addApplication(form: NewApplicationFormDto): Promise<FormSaveOutcome>;
  /**
   * Save a temporary terminal's launch configuration (#65) — "保存启动配置".
   *
   * Answers once the backend has written the entry *and* marked the session
   * saved, or with the reason it was refused. The terminal is not restarted,
   * and the session id does not change: the row the user was looking at keeps
   * its run, its output and its place, and gains a name that survives a
   * restart.
   */
  saveTerminal(sessionId: string, form: SaveTerminalFormDto): Promise<FormSaveOutcome>;
  /** Stop and remove a temporary terminal; retain it if stopping fails. */
  closeTerminal(sessionId: string): Promise<void>;
  closingSessionIds: ReadonlySet<string>;
  removeApplication(sessionId: string): Promise<FormSaveOutcome>;
}

/** Where the rendered sessions came from. */
export type SessionSource = "preview" | "loading" | "backend";

const EMPTY_LIVE_SNAPSHOT: SessionRegistrySnapshot = {
  phase: "loading",
  sessions: [],
  error: null,
  sessionRevision: 0,
  configReport: null,
  configReportError: null,
};
const EMPTY_SESSIONS: SessionView[] = [];

/**
 * Session Core remains the lifecycle source of truth. The registry
 * coordinator subscribes before reading `list_session_configs` and
 * `list_sessions`, then follows full state events. It reconciles those events
 * with the initial snapshot and only renders rows with validated config.
 *
 * The hook supplies typed Tauri operations to that injected backend boundary.
 * It renders an empty loading state until the first live snapshot is ready;
 * preview fixtures are used only when no backend has answered.
 */
export function useSessionRegistry(connection: BackendConnection): SessionRegistry {
  const live = connection.state === "connected";
  const [registryForConnection, setRegistryForConnection] = useState<{
    connection: BackendConnection;
    snapshot: SessionRegistrySnapshot;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorAt, setErrorAt] = useState(0);
  /**
   * Report a failed action. The timestamp travels beside the message because
   * the failure's only consumer — the status bar — decides between this and
   * its own notices by which one happened last, not by which kind it is.
   */
  const reportError = useCallback((message: string) => {
    setError(message);
    setErrorAt(Date.now());
  }, []);
  const closing = useRef(new Set<string>());
  const [closingSessionIds, setClosingSessionIds] = useState<ReadonlySet<string>>(new Set());
  /**
   * The running watch, for the two commands whose answers the event stream may
   * not have delivered yet (`createTerminal`, `closeTerminal`).
   */
  const controller = useRef<SessionRegistryController | null>(null);

  useEffect(installVerificationReload, []);

  useEffect(() => {
    if (!live) {
      controller.current = null;
      return;
    }
    const activeConnection = connection;
    let lastSessionRevision = 0;
    const watching = watchSessionRegistry(
      {
        // Four event names, one listener: a session's state, its arrival, its
        // configuration changing (#65) and its departure are all "what the
        // registry looks like now", and a second listener keyed to the same
        // lifecycle would only be a second chance to get the ordering wrong.
        subscribe: (receive) =>
          Promise.all([
            listen<unknown>(SESSION_STATE_CHANGED, (event) => receive(event.payload)),
            listen<unknown>(SESSION_CREATED, (event) => receive(event.payload)),
            listen<unknown>(SESSION_SAVED, (event) => receive(event.payload)),
            listen<unknown>(SESSION_REMOVED, (event) => receive(event.payload)),
          ]).then((unlisten) => () => {
            for (const stop of unlisten) stop();
          }),
        listConfigs: () =>
          readVerificationSnapshot("configs", () => invoke<unknown>("list_session_configs")),
        listSessions: () =>
          readVerificationSnapshot("runtimes", () => invoke<unknown>("list_sessions")),
        getConfigReport: () => invoke<unknown>("get_config_report"),
      },
      (snapshot) => {
        setRegistryForConnection({ connection: activeConnection, snapshot });
        if (snapshot.sessionRevision > lastSessionRevision) {
          lastSessionRevision = snapshot.sessionRevision;
          setError(null);
        }
      },
    );
    controller.current = watching;
    return () => {
      controller.current = null;
      watching.stop();
    };
  }, [connection, live]);

  // A new connection starts empty immediately, without briefly showing fixture
  // rows or a snapshot from an earlier connection. If the transport later
  // goes away, keep its last known session snapshot; transport availability
  // does not rewrite lifecycle truth.
  const snapshot = live
    ? registryForConnection?.connection === connection
      ? registryForConnection.snapshot
      : EMPTY_LIVE_SNAPSHOT
    : registryForConnection?.snapshot.phase === "ready"
      ? registryForConnection.snapshot
      : null;
  const source: SessionSource =
    snapshot === null ? "preview" : snapshot.phase === "ready" ? "backend" : "loading";
  const sessions: SessionView[] =
    source === "preview" ? FIXTURE_SESSIONS : (snapshot?.sessions ?? EMPTY_SESSIONS);
  const loading = source === "loading";
  const initializationError = loading ? (snapshot?.error ?? null) : null;
  const configReport = snapshot?.configReport ?? null;
  const configReportError = snapshot?.configReportError ?? null;

  const run = useCallback(
    (command: string, sessionId: string) => {
      // The result is deliberately ignored: a lifecycle command answers with
      // the post-operation snapshot, and the event that follows publishes the
      // same state to everyone (the window, the tray, a future scheduler).
      // Rendering from the event alone is what keeps them from disagreeing. The
      // two "open" actions return no state, so they share this path and its
      // error handling instead of growing another one.
      invoke(command, { sessionId }).catch((cause) => reportError(sessionErrorMessage(cause)));
    },
    [reportError],
  );

  /**
   * The one command whose answer the window *does* read (#62).
   *
   * Everything else renders from the event, because the event is the same
   * fact published to everyone. A creation is different in one respect: the
   * caller needs the new session's id to select it, and only the answer has
   * it — the event carries the configuration but no notion of "this is the one
   * you asked for". The pair is adopted into the watch as well, which is what
   * makes the row exist even if the event is still in flight.
   */
  const createTerminal = useCallback(
    async (cwd?: string): Promise<string | null> => {
      try {
        const raw = await invoke<unknown>("create_temporary_terminal", { cwd });
        if (!isCreatedSessionDto(raw)) {
          throw new Error("create_temporary_terminal 返回了无法识别的载荷");
        }
        controller.current?.adopt(raw);
        return raw.config.id;
      } catch (cause) {
        // A shell or directory that is not there is a real answer, not a
        // silent failure: it is the message the user needs (H06).
        reportError(sessionErrorMessage(cause));
        return null;
      }
    },
    [reportError],
  );

  /**
   * "添加应用" (#64).
   *
   * Like `createTerminal`, this is one of the two commands whose answer the
   * window reads: only the answer says *which* session was added, and only it
   * carries a refusal the form can place beside the field that caused it.
   */
  const addApplication = useCallback(
    async (form: NewApplicationFormDto): Promise<FormSaveOutcome> => {
      try {
        const raw = await invoke<unknown>("add_application", { form });
        if (!isCreatedSessionDto(raw)) {
          throw new Error("add_application 返回了无法识别的载荷");
        }
        controller.current?.adopt(raw);
        return { ok: true, sessionId: raw.config.id };
      } catch (cause) {
        if (isFormErrorDto(cause)) {
          return { ok: false, message: cause.message, field: cause.field };
        }
        return { ok: false, message: sessionErrorMessage(cause) };
      }
    },
    [],
  );

  /**
   * "保存启动配置" (#65).
   *
   * The answer is the same pair a creation answers with, and `adopt` applies it
   * the same way — which for an id the watch already has means correcting that
   * row rather than adding one, so a run that was just saved is never listed
   * twice. The runtime in the answer is the run the session already had, which
   * is what makes the answer proof that the terminal was not restarted.
   */
  const saveTerminal = useCallback(
    async (sessionId: string, form: SaveTerminalFormDto): Promise<FormSaveOutcome> => {
      try {
        const raw = await invoke<unknown>("save_terminal_config", { sessionId, form });
        if (!isCreatedSessionDto(raw)) {
          throw new Error("save_terminal_config 返回了无法识别的载荷");
        }
        controller.current?.adopt(raw);
        return { ok: true, sessionId: raw.config.id };
      } catch (cause) {
        if (isFormErrorDto(cause)) {
          return { ok: false, message: cause.message, field: cause.field };
        }
        return { ok: false, message: sessionErrorMessage(cause) };
      }
    },
    [],
  );

  const removeApplication = useCallback(async (sessionId: string): Promise<FormSaveOutcome> => {
    try {
      await invoke("remove_application", { sessionId });
      controller.current?.forget(sessionId);
      return { ok: true, sessionId };
    } catch (cause) {
      return { ok: false, message: sessionErrorMessage(cause) };
    }
  }, []);

  const closeTerminal = useCallback(
    async (sessionId: string) => {
      if (closing.current.has(sessionId)) return;
      closing.current.add(sessionId);
      setClosingSessionIds(new Set(closing.current));
      // The backend confirms the owned tree has ended before removal. A failed
      // stop must leave the row and its process ownership intact.
      const read = async () => {
        const raw = await invoke<unknown>("get_session", { sessionId });
        if (!isSessionRuntimeDto(raw)) throw new Error("无法读取终端状态，未关闭会话");
        return raw;
      };
      try {
        // The state machine accepts a stop from `running` alone (spec §5), so
        // asking from any other status is a refusal the close did not need: an
        // `error` row without a run is removable right away, and one with a run
        // is owed the removal refusal that says so.
        const runtime = await read();
        if (runtime.status === "running") {
          try {
            await invoke("stop_session", { sessionId });
          } catch (cause) {
            // The run read above may have ended on its own before the stop
            // landed — the state machine refuses that stop, but the settled
            // session is exactly as closable as it was. Only a still-live
            // status makes the refusal the answer to give the user.
            const current = await read();
            if (
              current.status === "running" ||
              current.status === "starting" ||
              current.status === "stopping"
            ) {
              throw cause;
            }
          }
        }
        await invoke("remove_session", { sessionId });
        controller.current?.forget(sessionId);
      } catch (cause) {
        reportError(sessionErrorMessage(cause));
      } finally {
        closing.current.delete(sessionId);
        setClosingSessionIds(new Set(closing.current));
      }
    },
    [reportError],
  );

  /**
   * Opening an application (#64), and what happened to its own window (#66).
   *
   * A third command whose answer the window reads, and for the same kind of
   * reason as the other two: "the application is running but its window would
   * not come forward" is a fact only this answer carries, and the user has to
   * be told it rather than left clicking a button that appears to do nothing
   * (story 48). Everything else about the open still arrives as an event.
   */
  const activate = useCallback(
    async (sessionId: string): Promise<ActivationOutcomeDto | null> => {
      try {
        const raw = await invoke<unknown>("activate_session", { sessionId });
        if (!isActivationOutcomeDto(raw)) {
          throw new Error("activate_session 返回了无法识别的载荷");
        }
        return raw;
      } catch (cause) {
        reportError(sessionErrorMessage(cause));
        return null;
      }
    },
    [reportError],
  );

  /**
   * Answering "which of these is the application?" (#67).
   *
   * The same reading as `activate`, because the answer is the same shape: an
   * association ends with a window step of its own, and the user is owed the
   * same truth about it — including when it could not be brought forward.
   */
  const resolveOpen = useCallback(
    async (
      sessionId: string,
      resolution: OpenResolutionDto,
    ): Promise<ActivationOutcomeDto | null> => {
      try {
        const raw = await invoke<unknown>("resolve_session_open", { sessionId, resolution });
        if (!isActivationOutcomeDto(raw)) {
          throw new Error("resolve_session_open 返回了无法识别的载荷");
        }
        return raw;
      } catch (cause) {
        reportError(sessionErrorMessage(cause));
        return null;
      }
    },
    [reportError],
  );

  return useMemo<SessionRegistry>(
    () => ({
      sessions,
      source,
      loading,
      live,
      error,
      errorAt,
      initializationError,
      configReport,
      configReportError,
      // Opening goes through the activation semantics (#64), not a bare
      // start: the same call a launch request handed to the Hub performs, so
      // the two entries cannot disagree about what "open" means.
      activate,
      resolveOpen,
      stop: (sessionId) => run("stop_session", sessionId),
      restart: (sessionId) => run("restart_session", sessionId),
      forceStop: (sessionId) => run("force_stop_session", sessionId),
      openUrl: (sessionId) => run("open_session_url", sessionId),
      openDirectory: (sessionId) => run("open_session_cwd", sessionId),
      createTerminal,
      addApplication,
      saveTerminal,
      closeTerminal,
      closingSessionIds,
      removeApplication,
    }),
    [
      sessions,
      source,
      loading,
      live,
      error,
      errorAt,
      initializationError,
      configReport,
      configReportError,
      run,
      activate,
      resolveOpen,
      createTerminal,
      addApplication,
      saveTerminal,
      closeTerminal,
      closingSessionIds,
      removeApplication,
    ],
  );
}
