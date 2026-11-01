import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import TitleBar from "../components/title-bar/TitleBar";
import Sidebar from "../components/sidebar/Sidebar";
import SessionHeader from "../components/session-header/SessionHeader";
import WorkspaceTabs from "../components/workspace/WorkspaceTabs";
import TerminalHost from "../components/terminal/TerminalHost";
import StandalonePanel from "../components/workspace/StandalonePanel";
import LogsPanel from "../components/logs/LogsPanel";
import DetailsPanel from "../components/details/DetailsPanel";
import StatusBar from "../components/status-bar/StatusBar";
import PortsWorkspace from "../components/ports/PortsWorkspace";
import SessionResources from "../components/resources/SessionResources";
import { usePortList } from "../components/ports/usePortList";
import { openableSessionId, type SidebarView } from "../state/ports";
import ConfigDiagnostics from "../components/config-diagnostics/ConfigDiagnostics";
import AddApplicationDialog from "../components/add-application/AddApplicationDialog";
import RemoveApplicationDialog from "../components/remove-application/RemoveApplicationDialog";
import SaveTerminalDialog from "../components/save-terminal/SaveTerminalDialog";
import OpenChoiceDialog from "../components/open-choice/OpenChoiceDialog";
import PortOccupancyDialog from "../components/port-occupancy/PortOccupancyDialog";
import type { NewApplicationFormDto, SaveTerminalFormDto, SessionConfigDto } from "../types/config";
import {
  filterSessions,
  groupSessions,
  initialSelectedSessionId,
  isReady,
  isStandalone,
  liveCounts,
  sidebarSummaryText,
  titlebarSummaryText,
} from "../state/derivations";
import { SESSION_ACTION_LABELS, type SessionAction } from "../state/actions";
import {
  afterOccupancyCheck,
  afterPromptChoice,
  beforeOccupancyCheck,
  describeOccupants,
} from "../state/port-occupancy";
import { DEFAULT_SELECTED_SESSION_ID, FIXTURE_GROUPS, FIXTURE_SESSIONS } from "../state/fixtures";
import { LIVE_GROUPS } from "../state/session-view";
import type { WorkspaceTab } from "../state/view";
import type { ListenerRowDto } from "../types/listen";
import { isPortOccupancyDto, PORT_OCCUPANCY_COMMAND } from "../types/port-occupancy";
import type { OpenChoiceDto, OpenResolutionDto } from "../types/runtime";
import { SESSION_FOCUS_REQUESTED, isSessionFocusRequestedDto } from "../types/tray";
import { SESSION_OPENED, isSessionOpenedDto } from "../types/launch";
import { copyPathToClipboard } from "./clipboard";
import { useBackendPing } from "./useBackendPing";
import { useDisplayAdvice } from "./useDisplayAdvice";
import { useApplicationDiscovery } from "./useApplicationDiscovery";
import { useSessionRegistry } from "./useSessionRegistry";
import { useMediaQuery } from "./useMediaQuery";
import "./App.css";

const NOTICE_TIMEOUT_MS = 4000;
/** Uptime text shows minutes and seconds; a slow tick keeps it fresh without
 * turning the whole shell into a per-second renderer (spec §14). */
const CLOCK_TICK_MS = 5000;

/**
 * The V2 workspace shell (T06 #7, wired to Session Core by T07 #8).
 *
 * Structure per docs/UI_STYLE_GUIDE.md: compact title bar, grouped session
 * sidebar, selected-session workspace (header + 终端/日志/详情 tabs, terminal
 * dominant), minimal status bar.
 *
 * Where the sessions come from is `useSessionRegistry`'s business: the backend's
 * registry when one is answering, the T06 fixture workspace otherwise. Either
 * way the shell renders the same `SessionView` shape, and the lifecycle
 * controls act on Session Core rather than on what the window happens to think.
 *
 * The Logs tab is the one surface that reads further than the view model: it
 * asks Session Core for the session's logging state and run history itself
 * (`useSessionLogs`), because nothing else renders them, and it labels the
 * values it had to fall back to the fixture for (T10).
 */
export default function App() {
  const connection = useBackendPing();
  const registry = useSessionRegistry(connection);
  // The form's own read-only question (#66), kept apart from the session
  // registry because a launch method that has not been saved is not a session.
  const recommendDisplay = useDisplayAdvice();
  const discovery = useApplicationDiscovery();
  const sessions = registry.sessions;

  // `#session=<id>` deep link (tray/restore surfaces can target a session).
  const [selectedId, setSelectedId] = useState(
    () =>
      initialSelectedSessionId(
        typeof window === "undefined" ? null : window.location.hash,
        FIXTURE_SESSIONS.map((session) => session.config.id),
      ) || DEFAULT_SELECTED_SESSION_ID,
  );
  const [tab, setTab] = useState<WorkspaceTab>("terminal");
  const [query, setQuery] = useState("");
  const [railView, setRailView] = useState<SidebarView>("sessions");
  const [notice, setNoticeText] = useState<string | null>(null);
  /**
   * When `notice` was last set. The status bar composes it against the
   * registry's action errors by recency: a failure stops covering the screen
   * the moment a newer piece of feedback exists, whatever surface produced it.
   */
  const [noticeAt, setNoticeAt] = useState(0);
  const setNotice = useCallback((message: string) => {
    setNoticeText(message);
    setNoticeAt(Date.now());
  }, []);
  const [now, setNow] = useState(() => new Date());
  const narrow = useMediaQuery("(max-width: 767px)");
  const [drawerOpen, setDrawerOpen] = useState(false);
  const ports = usePortList(
    railView,
    connection.state === "connected",
    sessions.map((session) => ({ id: session.config.id, name: session.config.name })),
    now.getTime(),
  );
  /** Bumped for each "新建 PowerShell" the workspace carried out (#62). */
  const [focusRequest, setFocusRequest] = useState(0);
  /** Whether the "添加应用" form is open (#64). */
  const [addApplicationOpen, setAddApplicationOpen] = useState(false);
  /** The terminal whose launch configuration is being saved (#65). */
  const [removeApplicationTarget, setRemoveApplicationTarget] = useState<SessionConfigDto | null>(
    null,
  );
  const [saveTerminalTarget, setSaveTerminalTarget] = useState<SessionConfigDto | null>(null);
  /**
   * The question an open raised, and the session it is about (#67).
   *
   * Held with the id rather than read from the selection when the user
   * answers: the workspace can move under an open dialog, and associating the
   * instance with whichever session happened to be selected afterwards would
   * associate the wrong entry.
   */
  const [openChoice, setOpenChoice] = useState<{
    sessionId: string;
    sessionName: string;
    choice: OpenChoiceDto;
  } | null>(null);
  /**
   * Who holds the configured port, asked before a start (#99).
   *
   * Held with the id rather than read from the selection when the user
   * answers: the workspace can move under an open dialog.
   */
  const [occupancyPrompt, setOccupancyPrompt] = useState<{
    sessionId: string;
    sessionName: string;
    port: number;
    mode: "occupied" | "unreadable";
    rows: ListenerRowDto[];
  } | null>(null);
  const occupancyChecks = useRef(new Set<string>());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(new Date()), CLOCK_TICK_MS);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (notice === null) return;
    const timer = window.setTimeout(() => {
      setNoticeText(null);
      // The timestamp goes with it: an expired notice stops being the newest
      // feedback, so a still-unresolved action error takes the line back.
      setNoticeAt(0);
    }, NOTICE_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [notice]);

  /**
   * Put the workspace on one session and the keyboard in its terminal.
   *
   * The three steps "新建 PowerShell" takes (#62), named so the two other
   * callers can take them too: a launch request that made a terminal (#63) and
   * a click on a tray row. All three mean "show me this one" — a user who
   * asked for a shell and got a window showing something else has not been
   * answered, and a tray row whose terminal the keyboard is not in is a look
   * that costs a second click.
   */
  const showSession = useCallback((sessionId: string) => {
    setRailView("sessions");
    setSelectedId(sessionId);
    setTab("terminal");
    setFocusRequest((request) => request + 1);
    setDrawerOpen(false);
  }, []);

  const openPortSession = useCallback((sessionId: string) => {
    setRailView("sessions");
    setSelectedId(sessionId);
    setDrawerOpen(false);
  }, []);

  /**
   * "Show me this session" — two requests, from two places, doing two things.
   *
   * A click on a tray row (T09 #10) is a request to *look at* a session: it
   * arrives as a selection and nothing else — no lifecycle command is implied,
   * no tab is switched, and a session name the workspace cannot resolve falls
   * back to the derived selection exactly as a stale deep link does. That is
   * the behaviour the tray shipped with, and this listener keeps it.
   *
   * A launch request that made a terminal (#63) asks for more, because the
   * user clicked their own PowerShell entry: the new terminal has to be showing
   * and holding the keyboard, or the entry did not do what it says. That one
   * arrives as `session-opened` and goes through `showSession`.
   *
   * The read beside the listeners is what a *cold start* needs. A shortcut that
   * starts the Hub is answered while the page is still loading, so its event is
   * published to a window with no listener yet; the backend keeps the ask
   * (`take_launch_focus`) and this reads it once, **after** subscribing — so a
   * request that arrives between the two is delivered by the event, and one
   * that arrived before is found here. A request caught by both halves shows
   * the same session twice, which is what showing it once means.
   *
   * Only registered once a backend answers. A tray exists only in the desktop
   * app, and in the browser preview there is no IPC channel to listen on — the
   * same reason `useSessionRegistry` registers its own listener there and
   * nowhere else.
   */
  useEffect(() => {
    if (connection.state !== "connected") return;
    const subscription = Promise.all([
      listen<unknown>(SESSION_FOCUS_REQUESTED, (event) => {
        if (isSessionFocusRequestedDto(event.payload)) {
          setSelectedId(event.payload.sessionId);
        }
      }),
      listen<unknown>(SESSION_OPENED, (event) => {
        if (isSessionOpenedDto(event.payload)) {
          showSession(event.payload.sessionId);
        }
      }),
    ]);
    void subscription
      .then(() => invoke<unknown>("take_launch_focus"))
      .then((pending) => {
        if (typeof pending === "string") showSession(pending);
      })
      // A window that cannot read this is still a usable window: the event
      // above is what a running Hub uses, and this is only the cold-start half.
      .catch(() => {});
    return () => {
      void subscription.then((unlisten) => {
        for (const stop of unlisten) stop();
      });
    };
  }, [connection.state, showSession]);

  const filtered = useMemo(() => filterSessions(sessions, query), [sessions, query]);
  const groups = useMemo(
    () => groupSessions(filtered, registry.source === "backend" ? LIVE_GROUPS : FIXTURE_GROUPS),
    [filtered, registry.source],
  );

  // The selection is derived rather than repaired. The workspace can change
  // under it — a deep link naming a fixture session, then a backend whose
  // config has no such id — and the honest rendering of "that session is not
  // here" is the first one that is, not a header for a session the rail cannot
  // show. `selectedId` keeps whatever the user last chose, so a session that
  // comes back is still selected.
  const selected = sessions.find((session) => session.config.id === selectedId) ?? sessions[0];
  const counts = useMemo(() => liveCounts(sessions), [sessions]);
  const diagnosticSessionCount = registry.configReport?.sessions.length ?? sessions.length;
  // The registry's action error speaks until something newer happens: an
  // error reported after the last notice keeps the line, and any notice the
  // user earns afterwards — a copy, a save, a log action — takes it back. A
  // tie goes to the error, which is also the case when no notice has been
  // set yet (`noticeAt` is 0).
  const statusNotice =
    registry.error !== null && noticeAt <= registry.errorAt
      ? registry.error
      : registry.loading && !registry.initializationError
        ? "正在同步会话…"
        : notice;

  /** Preview mode: actions render from the real lifecycle rules but perform
   * nothing, because there is no run behind them to act on. */
  const onPreviewAction = (label: string) => {
    setNotice(`预览模式 ·「${label}」需要连接到后端`);
  };

  /**
   * "新建 PowerShell" (#62): create, select and focus, in one gesture.
   *
   * Nothing is asked for first — no form, no dialog, no default session to
   * copy. The backend resolves the shell (`pwsh`, else Windows PowerShell),
   * opens it in the user's home directory and answers with the new session's
   * id; the workspace selects it and the terminal takes the keyboard, so the
   * next keystroke is already a command (stories 6–8).
   *
   * A failure is a message, not a row: the backend withdraws a terminal it
   * could not start, and what is left is the reason — a shell that is not on
   * this machine, or a directory that is not there (story 17).
   *
   * This is the same terminus as the shortcut's request (#63): both end in
   * `showSession`, so a terminal made by the button and one made by the user's
   * own PowerShell entry are shown the same way.
   */
  const onCreateTerminal = () => {
    if (!registry.live) {
      onPreviewAction(SESSION_ACTION_LABELS["new-session"]);
      return;
    }
    void registry.createTerminal().then((sessionId) => {
      if (sessionId === null) return;
      showSession(sessionId);
    });
  };

  /**
   * "添加应用" (#64): a form, then a saved launch configuration.
   *
   * A preview workspace has no backend to save to and no file to save into, so
   * the entry says that instead of opening a form whose save could only fail.
   * The dialog itself stays open on a refusal — the user's input is the thing
   * they need to fix, and closing would throw it away (story 31).
   */
  const onOpenAddApplication = () => {
    if (!registry.live) {
      onPreviewAction("添加应用");
      return;
    }
    setAddApplicationOpen(true);
    setDrawerOpen(false);
  };

  const onAddApplication = async (form: NewApplicationFormDto) => {
    // The answer goes back untouched on a refusal, so the dialog places the
    // message on the field the backend named; a success closes the form and
    // selects what was added.
    const result = await registry.addApplication(form);
    if (result.ok) {
      setAddApplicationOpen(false);
      setSelectedId(result.sessionId);
      setTab("terminal");
      setNotice(`已保存「${form.name}」，下次打开 Hub 仍然可用`);
    }
    return result;
  };

  /**
   * "保存启动配置" (#65): a name, then a saved launch configuration.
   *
   * The launch method is not on the form — it is what the terminal is already
   * running — so the only thing asked for is what the Hub cannot know. The
   * dialog stays open on a refusal, exactly as the add-application one does,
   * because the input is what the user needs to fix (story 31).
   *
   * The row is not selected from here: it is the row the user was already
   * looking at, and the save arrives as the same session with a new
   * configuration, so there is nothing to move the selection to.
   */
  const onOpenSaveTerminal = () => {
    if (!registry.live) {
      onPreviewAction(SESSION_ACTION_LABELS["save-config"]);
      return;
    }
    setSaveTerminalTarget(selected.config);
  };

  /**
   * What a session control was asked for.
   *
   * Every one of these but two is a named Session Core operation (T08 #9 added
   * the two "open" actions); a new session needs a config writer that does not
   * exist yet, so it says so rather than being a button that appears to work.
   *
   * 复制路径 is the other exception, and the only action that reads nothing from
   * the backend: the path is already in the config this header renders. It is
   * answered before the connection check so the preview workspace can serve it
   * too — what it copies is exactly what the metadata line is showing. The gate
   * that matters is `availableActions`': a session with no `cwd` is never
   * offered the control.
   */
  /**
   * Open a session, and say what happened to an application's own window.
   *
   * `activate` answers with the window step as well as the lifecycle one
   * (#66), and the step is the only thing here the events do not repeat: an
   * application that is running with no window to bring forward, or a Windows
   * refusal, is reported once and nowhere else. Everything else about the open
   * still arrives as a session event.
   */
  /** The name of a session by id, for a dialog that outlives the selection. */
  const configNameOf = (sessionId: string): string =>
    sessions.find((session) => session.config.id === sessionId)?.config.name ?? sessionId;

  const activate = (sessionId: string) => {
    void registry.activate(sessionId).then((outcome) => {
      if (outcome === null) return;
      // The Hub found something outside itself it will not decide about (#67):
      // nothing was started and nothing was associated, so there is no notice
      // to show — there is a question to ask.
      if (outcome.choice !== undefined) {
        setOpenChoice({
          sessionId,
          sessionName: configNameOf(sessionId),
          choice: outcome.choice,
        });
        return;
      }
      const notice = outcome?.window?.notice;
      if (notice) setNotice(notice);
    });
  };

  /**
   * Start, after saying who already holds the configured port (#99).
   *
   * The check is a read. It runs only when this click would start a stopped,
   * exited, or error session that has a port. Cancel never reaches `activate`.
   * A session with no port, one that is already running, and the preview path
   * stay on the path they had before.
   */
  const requestStart = (sessionId: string, previewLabel: string) => {
    const session = sessions.find((item) => item.config.id === sessionId);
    const step = beforeOccupancyCheck({
      live: registry.live,
      port: session?.config.port,
      status: session?.runtime.status ?? "stopped",
    });
    if (step.kind === "preview") {
      onPreviewAction(previewLabel);
      return;
    }
    if (step.kind === "activate") {
      activate(sessionId);
      return;
    }
    const port = session?.config.port;
    if (typeof port !== "number" || occupancyChecks.current.has(sessionId)) return;
    occupancyChecks.current.add(sessionId);
    const sessionName = configNameOf(sessionId);
    const showUnreadable = () => {
      setOccupancyPrompt({ sessionId, sessionName, port, mode: "unreadable", rows: [] });
    };
    void invoke<unknown>(PORT_OCCUPANCY_COMMAND, { sessionId })
      .then((raw) => {
        if (!isPortOccupancyDto(raw)) {
          showUnreadable();
          return;
        }
        const follow = afterOccupancyCheck(raw);
        if (follow.kind === "activate") {
          activate(sessionId);
          return;
        }
        setOccupancyPrompt({
          sessionId,
          sessionName,
          port: raw.port ?? port,
          mode: follow.mode,
          rows: follow.mode === "occupied" ? follow.rows : [],
        });
      })
      .catch(showUnreadable)
      .finally(() => {
        occupancyChecks.current.delete(sessionId);
      });
  };

  const cancelOccupancy = () => {
    if (afterPromptChoice("cancel") === "stay") setOccupancyPrompt(null);
  };

  const continueOccupancy = () => {
    if (occupancyPrompt === null) return;
    const sessionId = occupancyPrompt.sessionId;
    setOccupancyPrompt(null);
    if (afterPromptChoice("continue") === "activate") activate(sessionId);
  };

  /**
   * Answer the question (#67).
   *
   * Three answers carry three different truths, and the user is owed the one
   * that happened: an association that found a window behind it says so, an
   * association that could not bring it forward says *that* (the window step's
   * own notice), and starting a new copy says what it left alone.
   */
  const resolveOpen = (resolution: OpenResolutionDto) => {
    if (openChoice === null) return;
    const { sessionId } = openChoice;
    setOpenChoice(null);
    void registry.resolveOpen(sessionId, resolution).then((outcome) => {
      if (outcome === null) return;
      const fallback =
        resolution.kind === "new"
          ? "已由 Hub 启动它自己的一份；此前运行的那份没有被结束"
          : "已关联正在运行的实例，Hub 没有另启一份";
      setNotice(outcome.window?.notice ?? fallback);
    });
  };

  const onSessionAction = (action: SessionAction) => {
    const label = SESSION_ACTION_LABELS[action];
    if (action === "copy-path") {
      if (selected.config.cwd === undefined) {
        setNotice("该会话未配置工作目录，没有可复制的路径");
        return;
      }
      copyPathToClipboard(selected.config.cwd, setNotice);
      return;
    }
    if (!registry.live) {
      onPreviewAction(label);
      return;
    }
    switch (action) {
      // Opening, not starting (#64): the activation semantics are what keep a
      // second click from creating a second run of the same application.
      case "start":
        requestStart(selected.config.id, label);
        break;
      case "stop":
        registry.stop(selected.config.id);
        break;
      case "restart":
        registry.restart(selected.config.id);
        break;
      case "force-stop":
        registry.forceStop(selected.config.id);
        break;
      case "open-url":
        registry.openUrl(selected.config.id);
        break;
      case "open-directory":
        registry.openDirectory(selected.config.id);
        break;
      case "save-config":
        onOpenSaveTerminal();
        break;
      case "remove-application":
        setRemoveApplicationTarget(selected.config);
        break;
      case "remove-session":
        void registry.closeTerminal(selected.config.id);
        break;
      default:
        setNotice(`「${label}」尚未接入`);
    }
  };

  /** The form, in whichever branch of the shell is on screen. */
  const addApplicationDialog = addApplicationOpen ? (
    <AddApplicationDialog
      onSubmit={onAddApplication}
      onRecommendDisplay={recommendDisplay}
      onPickPath={discovery.pick}
      onScanDirectory={discovery.scan}
      onClose={() => setAddApplicationOpen(false)}
    />
  ) : null;

  /**
   * The save form (#65), holding the terminal it was opened for.
   *
   * The target is captured here rather than read from the selection when the
   * form is submitted: the workspace can move under an open dialog, and saving
   * whichever session happened to be selected at that moment would save a
   * terminal the user never asked about.
   */
  const openChoiceDialog =
    openChoice === null ? null : (
      <OpenChoiceDialog
        sessionName={openChoice.sessionName}
        choice={openChoice.choice}
        onResolve={resolveOpen}
        onClose={() => setOpenChoice(null)}
      />
    );

  const occupancyDialog =
    occupancyPrompt === null ? null : (
      <PortOccupancyDialog
        key={occupancyPrompt.sessionId}
        sessionName={occupancyPrompt.sessionName}
        port={occupancyPrompt.port}
        mode={occupancyPrompt.mode}
        lines={describeOccupants(
          occupancyPrompt.rows,
          sessions.map((session) => ({ id: session.config.id, name: session.config.name })),
          occupancyPrompt.sessionId,
        )}
        onContinue={continueOccupancy}
        onClose={cancelOccupancy}
      />
    );

  const saveTerminalDialog =
    saveTerminalTarget === null ? null : (
      <SaveTerminalDialog
        config={saveTerminalTarget}
        onSubmit={async (form: SaveTerminalFormDto) => {
          const result = await registry.saveTerminal(saveTerminalTarget.id, form);
          if (result.ok) {
            setSaveTerminalTarget(null);
            setNotice(`已保存「${form.name}」，下次打开 Hub 仍然可用`);
          }
          return result;
        }}
        onClose={() => setSaveTerminalTarget(null)}
      />
    );

  const removeApplicationDialog =
    removeApplicationTarget === null ? null : (
      <RemoveApplicationDialog
        key={removeApplicationTarget.id}
        config={removeApplicationTarget}
        onSubmit={async () => {
          const result = await registry.removeApplication(removeApplicationTarget.id);
          if (result.ok) {
            setRemoveApplicationTarget(null);
            setNotice(`已从受管名单移除「${removeApplicationTarget.name}」`);
          }
          return result;
        }}
        onClose={() => setRemoveApplicationTarget(null)}
      />
    );

  return (
    <div className="app-shell">
      <TitleBar
        summary={titlebarSummaryText(counts)}
        pill={connection.state === "unavailable" ? "preview" : "connected"}
        narrow={narrow}
        drawerOpen={drawerOpen}
        onToggleDrawer={() => setDrawerOpen((open) => !open)}
        onNotice={setNotice}
      />
      <div className={`app-main${narrow ? " app-main--narrow" : ""}`}>
        {narrow && drawerOpen && (
          <div
            className="app-main__backdrop"
            aria-hidden="true"
            onClick={() => setDrawerOpen(false)}
          />
        )}
        <div className={`app-main__rail${narrow && drawerOpen ? " app-main__rail--open" : ""}`}>
          <Sidebar
            groups={groups}
            selectedId={selected?.config.id ?? ""}
            now={now}
            summary={sidebarSummaryText(counts)}
            query={query}
            onQueryChange={setQuery}
            onSelect={(sessionId) => {
              setSelectedId(sessionId);
              setDrawerOpen(false);
            }}
            onAdd={onCreateTerminal}
            onAddApplication={onOpenAddApplication}
            view={railView}
            onViewChange={setRailView}
            portSummary={ports.summary}
            portQuery={ports.query}
            onPortQueryChange={ports.setQuery}
            portGroups={ports.groups}
            selectedPortKey={ports.selected?.key ?? ""}
            onSelectPort={ports.select}
            portEmpty={ports.empty}
          />
        </div>
        <section className="workspace">
          <div
            className={`workspace__session${selected === undefined ? " workspace__session--empty" : ""}`}
            hidden={railView === "ports"}
          >
            {selected === undefined ? (
              <>
                {registry.loading && (
                  <div
                    className={`workspace__registry-status${registry.initializationError ? " workspace__registry-status--error" : ""}`}
                    role={registry.initializationError ? "alert" : "status"}
                    aria-label="会话同步状态"
                  >
                    <div className="workspace__registry-status-heading">
                      <span
                        className={`pip ${registry.initializationError ? "pip--err" : "pip--warn"}`}
                        aria-hidden="true"
                      />
                      <strong>
                        {registry.initializationError ? "暂时无法读取会话状态" : "正在同步会话…"}
                      </strong>
                    </div>
                    <p>
                      {registry.initializationError
                        ? `${registry.initializationError} · 正在自动重试。`
                        : "正在连接到后台并读取已配置会话。"}
                    </p>
                  </div>
                )}
                <ConfigDiagnostics
                  report={registry.configReport}
                  error={registry.configReportError}
                  sessionCount={diagnosticSessionCount}
                />
                {!registry.loading && (
                  <div className="workspace__empty-hint">
                    <p className="workspace__empty-title">还没有会话</p>
                    <p>
                      {narrow
                        ? "请打开左上角会话列表，点击“添加应用”保存常用应用，或点击“新建 PowerShell”直接打开终端。"
                        : "首次使用，请点击左下方“添加应用”保存常用应用，或点击“新建 PowerShell”直接打开终端。"}
                    </p>
                    <p>添加的应用下次打开仍在；临时终端只在主动保存启动配置后保留。</p>
                  </div>
                )}
              </>
            ) : (
              <>
                <SessionHeader
                  config={selected.config}
                  runtime={selected.runtime}
                  busy={selected.busy ?? false}
                  ready={isReady(selected.runtime)}
                  now={now}
                  onAction={onSessionAction}
                  closing={registry.closingSessionIds.has(selected.config.id)}
                />
                <ConfigDiagnostics
                  report={registry.configReport}
                  error={registry.configReportError}
                  sessionCount={diagnosticSessionCount}
                />
                {registry.live && selected.runtime.status === "running" && (
                  <SessionResources key={selected.config.id} sessionId={selected.config.id} />
                )}
                <WorkspaceTabs active={tab} onChange={setTab} />
                <div className="workspace__content">
                  {tab === "terminal" &&
                    // A standalone-window application has no Hub-side stream to
                    // render (#66): the pane says where its console is instead of
                    // drawing an empty terminal for a console the Hub does not own.
                    (isStandalone(selected.config) ? (
                      <StandalonePanel
                        session={selected}
                        onActivate={() =>
                          requestStart(selected.config.id, `打开 ${selected.config.name}`)
                        }
                      />
                    ) : (
                      <TerminalHost
                        session={selected}
                        live={registry.live}
                        focusRequest={focusRequest}
                        onStart={() =>
                          requestStart(selected.config.id, `启动 ${selected.config.name}`)
                        }
                      />
                    ))}
                  {/* Keyed by session so the Logs tab's own state — the pending
                retention question above all — belongs to one session. */}
                  {tab === "logs" && (
                    <LogsPanel key={selected.config.id} session={selected} onNotice={setNotice} />
                  )}
                  {tab === "details" && (
                    <DetailsPanel
                      session={selected}
                      sessions={sessions}
                      onSaveTerminal={onOpenSaveTerminal}
                      closing={registry.closingSessionIds.has(selected.config.id)}
                    />
                  )}
                </div>
              </>
            )}
          </div>
          {railView === "ports" && (
            <PortsWorkspace
              connected={connection.state === "connected"}
              rows={ports.filtered}
              selected={ports.selected}
              caption={ports.caption}
              empty={ports.empty}
              inProgress={ports.inProgress}
              onSelect={ports.select}
              onRefresh={ports.refresh}
              onOpenSession={openPortSession}
              openableSessionId={(row) =>
                openableSessionId(
                  row,
                  sessions.map((session) => ({ id: session.config.id, name: session.config.name })),
                )
              }
            />
          )}
        </section>
      </div>
      <StatusBar counts={counts} connection={connection} notice={statusNotice} />
      {addApplicationDialog}
      {saveTerminalDialog}
      {removeApplicationDialog}
      {openChoiceDialog}
      {occupancyDialog}
    </div>
  );
}
