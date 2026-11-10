import { describe, expect, it } from "vitest";

import type { EffectiveLoggingDto, SessionConfigDto } from "../types/config";
import type { RunRecordDto, RuntimeEffectiveLoggingDto, SessionRuntimeDto } from "../types/runtime";
import {
  availableActions,
  bufferDiscardNotice,
  closeMechanics,
  dependenciesOf,
  filterSessions,
  formatDuration,
  groupSessions,
  headerCallout,
  healthReading,
  initialSelectedSessionId,
  isLive,
  isReady,
  liveCounts,
  logModeLabel,
  logModeToken,
  logSourceLabel,
  loggingHeadline,
  metadataPairs,
  acceptsTerminalInput,
  ptyChromeLabel,
  runOutcome,
  stoppedHint,
  runOutcomeBadge,
  sidebarRowMeta,
  sidebarSummaryText,
  statusLabel,
  statusTone,
  titlebarSummaryText,
  typeLabel,
} from "./derivations";
import { FIXTURE_GROUPS } from "./fixtures";
import { sessionsFromLive, type SessionView } from "./session-view";

function config(overrides: Partial<SessionConfigDto> = {}): SessionConfigDto {
  return {
    id: "sillytavern",
    name: "SillyTavern",
    sessionType: "service",
    cwd: "D:/Tools/SillyTavern",
    command: "node server.js",
    url: "http://127.0.0.1:8000",
    port: 8000,
    purpose: "聊天前端",
    closeImpact: "可停止；打开的聊天页会失联。",
    display: "internal",
    lifecycle: "managed",
    logging: { mode: "always", source: "captured" },
    ...overrides,
  };
}

function runtime(overrides: Partial<SessionRuntimeDto> = {}): SessionRuntimeDto {
  return {
    sessionId: "sillytavern",
    status: "running",
    pid: 18420,
    runId: "a91c",
    external: false,
    startedAt: "2026-09-28T05:00:00Z",
    ptyAttached: true,
    logging: { mode: "always", source: "captured", external_path: undefined },
    buffer: { bytes: 4096, lines: 8, droppedBytes: 0 },
    ...overrides,
  };
}

/** Config-side logging block in the runtime's wire casing (see runtime.ts). */
function toRuntimeLogging(logging: EffectiveLoggingDto): RuntimeEffectiveLoggingDto {
  return { mode: logging.mode, source: logging.source, external_path: logging.externalPath };
}

/**
 * A fixture whose snapshot logging is seeded from its config — mirroring the
 * backend, where `SessionRuntime::stopped` takes the validated effective
 * logging. Tests that need the two to diverge (a live run overriding the
 * config) pass an explicit `runtime` override.
 */
function fixture(overrides: Partial<SessionView> = {}): SessionView {
  const resolved = overrides.config ?? config();
  return {
    config: resolved,
    runtime: runtime({
      logging: toRuntimeLogging(resolved.logging),
      ...overrides.runtime,
    }),
    runs: overrides.runs ?? [],
    group: overrides.group ?? "ai",
    busy: overrides.busy,
    dependsOn: overrides.dependsOn,
    lines: overrides.lines,
  };
}

/** One run record as the wire writes it (a live run has `null` where a
 * finished one has an end and a code). */
function runRecord(): RunRecordDto {
  return {
    runId: "a91c",
    sessionId: "sillytavern",
    startedAt: "2026-09-28T05:00:00Z",
    endedAt: null,
    exitCode: null,
    pid: 18420,
    logMode: "always",
    logSource: "captured",
    logFile: null,
  };
}

const NOW = new Date("2026-09-28T08:14:30Z");

/**
 * A stopped session as the backend actually writes one: every absent optional
 * is `null`, not a missing key (see the wire note in `types/runtime.ts`).
 *
 * These exist because the T06 fixtures used `undefined`, so nothing exercised
 * the real shape until T07 #8 rendered live snapshots — where `pid: null` came
 * out as a literal "PID null" and a `null` `lastError` was dereferenced.
 */
const WIRE_STOPPED: Partial<SessionRuntimeDto> = {
  status: "stopped",
  pid: null,
  runId: null,
  startedAt: null,
  exitCode: null,
  ptyAttached: false,
  logging: { mode: "off", source: "none", external_path: null },
  lastError: null,
};

describe("reading the wire's nulls", () => {
  it("renders an absent PID as a dash, not as `PID null`", () => {
    const pairs = metadataPairs(config(), runtime(WIRE_STOPPED), NOW);
    expect(pairs.find((pair) => pair.label === "PID")?.value).toBe("—");
  });

  it("omits uptime for a session that has never started", () => {
    const pairs = metadataPairs(config(), runtime(WIRE_STOPPED), NOW);
    expect(pairs.some((pair) => pair.label === "up")).toBe(false);
  });

  it("shows the close-impact callout rather than crashing on a null lastError", () => {
    // `headerCallout` dereferenced `lastError` before checking it, so a null
    // one threw the render rather than falling through.
    expect(() => headerCallout(config(), runtime(WIRE_STOPPED))).not.toThrow();
    expect(headerCallout(config(), runtime(WIRE_STOPPED))).toBeNull();
  });

  it("reports a failed run's error when lastError is present", () => {
    const callout = headerCallout(
      config(),
      runtime({
        ...WIRE_STOPPED,
        status: "error",
        lastError: { operation: "run", message: "exit code 1" },
      }),
    );
    expect(callout).toEqual({ kind: "error", title: "上次错误", text: "exit code 1" });
  });

  it("treats a run with no end as running, whether null or missing", () => {
    expect(runOutcome({ ...runRecord(), endedAt: null })).toBe("running");
    expect(runOutcome({ ...runRecord(), endedAt: undefined })).toBe("running");
  });

  it("reports an ended run's outcome", () => {
    expect(runOutcome({ ...runRecord(), endedAt: "2026-09-28T06:00:00Z", exitCode: 0 })).toBe("ok");
    expect(runOutcome({ ...runRecord(), endedAt: "2026-09-28T06:00:00Z", exitCode: 3 })).toBe(
      "error",
    );
  });
});

describe("statusTone / statusLabel / typeLabel / isLive", () => {
  it("maps lifecycle states to the reference's tones and labels", () => {
    expect(statusTone("running")).toBe("run");
    expect(statusTone("running", true)).toBe("busy");
    expect(statusTone("starting")).toBe("warn");
    expect(statusTone("stopping")).toBe("warn");
    expect(statusTone("error")).toBe("err");
    expect(statusTone("stopped")).toBe("idle");
    expect(statusTone("exited")).toBe("idle");
    expect(statusLabel("running")).toBe("Running");
    expect(statusLabel("running", true)).toBe("Busy");
    expect(statusLabel("running", false, true)).toBe("Ready");
    expect(statusLabel("stopped")).toBe("Stopped");
    expect(statusLabel("starting")).toBe("Starting");
    expect(typeLabel("service")).toBe("Service");
    expect(typeLabel("terminal")).toBe("Terminal");
    expect(isLive("stopping")).toBe(true);
    expect(isLive("exited")).toBe(false);
  });
});

describe("formatDuration", () => {
  it("formats like the reference: hours, unpadded minutes/seconds", () => {
    expect(formatDuration("2026-09-28T08:13:45Z", NOW)).toBe("45s");
    expect(formatDuration("2026-09-28T08:02:25Z", NOW)).toBe("12m 5s");
    expect(formatDuration("2026-09-28T05:00:00Z", NOW)).toBe("3h 14m");
  });
});

describe("liveCounts / summaries", () => {
  const sessions = [
    fixture(),
    fixture({
      config: config({ id: "comfyui" }),
      runtime: runtime({ sessionId: "comfyui" }),
      busy: true,
    }),
    fixture({
      config: config({ id: "pwsh", sessionType: "terminal" }),
      runtime: runtime({ sessionId: "pwsh", status: "stopped" }),
    }),
  ];

  it("counts running and busy sessions separately", () => {
    expect(liveCounts(sessions)).toEqual({ total: 3, running: 2, busy: 1 });
  });

  it("words the sidebar and title-bar summaries", () => {
    const counts = liveCounts(sessions);
    expect(sidebarSummaryText(counts)).toBe("2 运行 · 1 忙碌 · 3 会话");
    expect(titlebarSummaryText(counts)).toBe("2 运行 · 1 busy");
    const calm = liveCounts([fixture()]);
    expect(sidebarSummaryText(calm)).toBe("1 运行 · 1 会话");
    expect(titlebarSummaryText(calm)).toBe("1 运行");
  });
});

describe("sessionsFromLive", () => {
  it("files a temporary terminal under its own group, and nothing else there", () => {
    const views = sessionsFromLive(
      [config({ id: "svc" }), config({ id: "term", sessionType: "terminal", temporary: true })],
      [],
    );

    expect(views.map((view) => view.group)).toEqual(["configured", "temporary"]);
    // A session whose snapshot has not been read is rendered stopped, which is
    // what makes a row that just appeared renderable before its first state
    // event lands (`stoppedRuntime`).
    expect(views[1]?.runtime.status).toBe("stopped");
  });
});

describe("availableActions", () => {
  // Removing is a temporary terminal's action, and only once it has ended
  // (#62): a running one still owns a process tree, and a configured one
  // belongs to the config file.
  it("offers removal for an ended temporary terminal and nobody else", () => {
    const ended = runtime({ status: "exited" });
    const stopped = runtime({ status: "stopped" });
    expect(availableActions(config({ temporary: true }), ended).remove).toBe(true);
    expect(availableActions(config({ temporary: true }), stopped).remove).toBe(true);
    expect(availableActions(config({ temporary: true }), runtime()).remove).toBe(false);
    expect(
      availableActions(config({ temporary: true }), runtime({ status: "starting" })).remove,
    ).toBe(false);
    expect(availableActions(config(), ended).remove).toBe(false);
    expect(availableActions(config({ temporary: false }), ended).remove).toBe(false);
  });

  // `error` is not "ended": a stop that could not confirm the terminal's tree
  // was gone reports it while still owning that tree, so the control that
  // would drop the last handle accounting for it must not appear (#62's
  // 不绕过归属). The backend's `removable` applies the same rule.
  it("withholds removal from a temporary terminal in an error state", () => {
    const failed = runtime({ status: "error" });
    expect(availableActions(config({ temporary: true }), failed).remove).toBe(false);
    // …while Start, a legal move from that state, stays offered.
    expect(availableActions(config({ temporary: true }), failed).start).toBe(true);
  });

  // A run the Hub does not own cannot be stopped or restarted by it (#66), and
  // the backend refuses both — so the controls are absent rather than offered
  // and rejected (spec #59 decision 12).
  it("withholds Stop and Restart from a run the Hub does not manage", () => {
    const independent = config({ display: "window", lifecycle: "independent" });
    const running = availableActions(independent, runtime());

    expect(running.start).toBe(false); // it is already running
    expect(running.stop).toBe(false);
    expect(running.forceStop).toBe(false);
    expect(availableActions(independent, runtime({ status: "stopped" })).restart).toBe(false);
    // Opening it stays: launching an application is what the entry is for.
    expect(availableActions(independent, runtime({ status: "stopped" })).start).toBe(true);
  });

  it("keeps the ordinary controls for a standalone run the user asked the Hub to manage", () => {
    const managed = config({ display: "window", lifecycle: "managed" });
    const running = availableActions(managed, runtime());

    expect(running.stop).toBe(true);
    expect(running.forceStop).toBe(true);
  });

  // An instance the Hub associated rather than started is the same answer from
  // the other direction (#67): whatever the entry's lifecycle setting says —
  // that setting is about the Hub's own runs — the Hub did not start this one,
  // and the backend refuses to stop it. The controls are absent, and the flag
  // beside them lets the header explain this reason rather than the wrong one.
  it("withholds Stop and Restart from an instance the Hub only associated", () => {
    const managed = config({ display: "window", lifecycle: "managed" });
    const associated = availableActions(managed, runtime({ external: true }));

    expect(associated.associated).toBe(true);
    expect(associated.managed).toBe(true); // the configuration is unchanged
    expect(associated.stop).toBe(false);
    expect(associated.forceStop).toBe(false);
    expect(associated.restart).toBe(false);
    // Opening stays, and is the one control that does work: it brings the
    // application's own window forward.
    expect(associated.start).toBe(false); // the instance is running
    expect(availableActions(managed, runtime({ status: "stopped" })).start).toBe(true);
  });

  // Editing is a configured service's action, offered while it is settled: a
  // live run was started from the configuration being edited, and a temporary
  // terminal has no file entry (its "保存启动配置" writes one).
  it("offers editing to a settled configured service, and nobody else", () => {
    expect(availableActions(config(), runtime({ status: "stopped" })).edit).toBe(true);
    expect(availableActions(config(), runtime({ status: "exited" })).edit).toBe(true);
    // A failed start left nothing running; the configuration is still the
    // user's to fix.
    expect(availableActions(config(), runtime({ status: "error" })).edit).toBe(true);
    expect(availableActions(config(), runtime()).edit).toBe(false);
    expect(availableActions(config(), runtime({ status: "starting" })).edit).toBe(false);
    expect(availableActions(config(), runtime({ status: "stopping" })).edit).toBe(false);
    expect(availableActions(config({ temporary: true }), runtime({ status: "stopped" })).edit).toBe(
      false,
    );
    const terminal = config({ sessionType: "terminal", shell: "pwsh" });
    expect(availableActions(terminal, runtime({ status: "stopped" })).edit).toBe(false);
  });

  // Saving a terminal's launch configuration (#65) is the one control that is
  // offered *while the terminal runs*: it is the terminal in front of the user
  // that is worth keeping, and ending it first would throw away the thing being
  // saved. It is gone as soon as the session is saved, because `temporary` is
  // the "not saved yet" flag.
  it("offers saving for a temporary terminal in any state, and never for a saved one", () => {
    for (const status of ["running", "stopped", "exited", "error"] as const) {
      expect(availableActions(config({ temporary: true }), runtime({ status })).saveConfig).toBe(
        true,
      );
    }
    expect(availableActions(config(), runtime()).saveConfig).toBe(false);
    expect(availableActions(config({ temporary: false }), runtime()).saveConfig).toBe(false);
  });

  it("offers Start only when idle, Stop only when live", () => {
    const running = availableActions(config(), runtime());
    expect(running.start).toBe(false);
    expect(running.stop).toBe(true);
    const stopped = availableActions(config(), runtime({ status: "stopped" }));
    expect(stopped.start).toBe(true);
    expect(stopped.stop).toBe(false);
    expect(availableActions(config(), runtime({ status: "stopping" })).start).toBe(false);
  });

  it("keeps Stop visible but inert while already stopping", () => {
    const stopping = availableActions(config(), runtime({ status: "stopping" }));
    expect(stopping.stop).toBe(true);
    expect(stopping.stopDisabled).toBe(true);
    expect(availableActions(config(), runtime()).stopDisabled).toBe(false);
  });

  it("withholds Restart until the previous run is settled (spec §5 rule 2)", () => {
    expect(availableActions(config(), runtime()).restart).toBe(true);
    expect(availableActions(config(), runtime({ status: "stopped" })).restart).toBe(true);
    expect(availableActions(config(), runtime({ status: "starting" })).restart).toBe(false);
    expect(availableActions(config(), runtime({ status: "stopping" })).restart).toBe(false);
  });

  it("offers the two open actions only where the config has a target", () => {
    const actions = availableActions(config(), runtime());
    expect(actions.directory).toBe(true);
    expect(actions.openUrl).toBe("http://127.0.0.1:8000");
    // A session with nothing to open offers no button, rather than one the
    // backend would answer with "there is no url in its configuration".
    expect(availableActions(config({ url: undefined }), runtime()).openUrl).toBeUndefined();
    expect(availableActions(config({ cwd: undefined }), runtime()).directory).toBe(false);
    const terminal = config({
      sessionType: "terminal",
      url: undefined,
      port: undefined,
      shell: "pwsh",
    });
    expect(availableActions(terminal, runtime()).openUrl).toBeUndefined();
    expect(availableActions(terminal, runtime()).directory).toBe(true);
  });

  it("offers 复制路径 on the same gate as the directory it copies", () => {
    // 目录/打开目录 and 复制路径 act on one thing, so there is one answer to
    // "is there a directory": a session with no `cwd` shows neither, rather
    // than a control whose only possible answer is "there is no working
    // directory".
    expect(availableActions(config(), runtime()).copyPath).toBe(true);
    expect(availableActions(config({ cwd: undefined }), runtime()).copyPath).toBe(false);
    // The gate is the directory's, not the session type's: a shell has a path
    // to copy too, and needs no URL to be offered one.
    const shell = config({ sessionType: "terminal", url: undefined, shell: "pwsh" });
    expect(availableActions(shell, runtime()).copyPath).toBe(true);
  });

  it("scopes force stop to the live window, including Stopping", () => {
    expect(availableActions(config(), runtime()).forceStop).toBe(true);
    expect(availableActions(config(), runtime({ status: "stopping" })).forceStop).toBe(true);
    expect(availableActions(config(), runtime({ status: "stopped" })).forceStop).toBe(false);
  });
});

describe("isReady", () => {
  it("calls a service ready when its port answers", () => {
    expect(isReady(runtime({ health: { processAlive: true, portOpen: true } }))).toBe(true);
  });

  it("does not call a running service ready while nothing is listening", () => {
    // The two facts stay apart: the process is alive, the service is not up.
    expect(isReady(runtime({ health: { processAlive: true, portOpen: false } }))).toBe(false);
    expect(isReady(runtime({ health: { processAlive: false, portOpen: false } }))).toBe(false);
  });

  it("does not call a service ready on a port its own process is not holding", () => {
    // Something is listening on the configured port, but not this run's
    // process — so this session is not the thing answering, and `Ready` would
    // be claiming a service that is not there. The Details row says both facts,
    // and the badge stays on the lifecycle state.
    const reading = { processAlive: false, portOpen: true };
    expect(isReady(runtime({ health: reading }))).toBe(false);
    expect(healthReading(runtime({ health: reading }))).toBe("监听中 · 本会话进程已退出");
  });

  it("never calls a session ready before anything has been probed", () => {
    // `null` is "we did not check", not "the port is closed" — so the badge
    // falls back to what the state machine knows.
    expect(isReady(runtime({ status: "starting", health: null }))).toBe(false);
    expect(isReady(runtime({ status: "running", health: null, ptyAttached: false }))).toBe(false);
  });

  it("calls a running attached terminal ready", () => {
    // The reference labels it `Ready`; a shell that is attached and accepting
    // input is the terminal's equivalent of a port that answers.
    expect(isReady(runtime({ health: null, ptyAttached: true }))).toBe(true);
    expect(isReady(runtime({ status: "stopped", health: null, ptyAttached: true }))).toBe(false);
  });

  it("reads the wire's nulls, not just missing keys", () => {
    expect(isReady(runtime(WIRE_STOPPED))).toBe(false);
  });
});

describe("healthReading", () => {
  it("says the port's answer and the process's as separate facts", () => {
    expect(healthReading(runtime({ health: { processAlive: true, portOpen: true } }))).toBe(
      "监听中",
    );
    expect(healthReading(runtime({ health: { processAlive: true, portOpen: false } }))).toBe(
      "未监听",
    );
    expect(healthReading(runtime({ health: { processAlive: false, portOpen: false } }))).toBe(
      "未监听 · 本会话进程已退出",
    );
  });

  it("reports nothing at all when nothing has been probed", () => {
    expect(healthReading(runtime({ health: null }))).toBeUndefined();
    expect(healthReading(runtime(WIRE_STOPPED))).toBeUndefined();
    expect(healthReading(runtime({ health: undefined }))).toBeUndefined();
  });
});

describe("acceptsTerminalInput", () => {
  it("lets a running interactive terminal take typing", () => {
    expect(
      acceptsTerminalInput(config({ sessionType: "terminal" }), runtime({ ptyAttached: true })),
    ).toBe(true);
  });

  it("refuses input for a terminal that is not running", () => {
    expect(
      acceptsTerminalInput(config({ sessionType: "terminal" }), runtime({ ptyAttached: false })),
    ).toBe(false);
  });

  it("refuses input while a terminal is stopping, even though it is still attached", () => {
    // `stop` sets `Stopping` and waits out the grace period before the run
    // ends, and the flag is only cleared when it does — so for that window the
    // snapshot reports an attachment the backend will not take input for.
    expect(
      acceptsTerminalInput(
        config({ sessionType: "terminal" }),
        runtime({ status: "stopping", ptyAttached: true }),
      ),
    ).toBe(false);
  });

  it("accepts input only for a running attached terminal", () => {
    expect(
      acceptsTerminalInput(
        config({ sessionType: "terminal" }),
        runtime({ status: "running", ptyAttached: true }),
      ),
    ).toBe(true);
  });

  it("refuses input for a service, whatever the snapshot claims", () => {
    // The backend refuses a service at `terminal_write` because a supervised run
    // has no attached stdin; the view must not offer what will be refused, so
    // the session type gates it independently of the attachment flag.
    expect(acceptsTerminalInput(config(), runtime({ ptyAttached: true }))).toBe(false);
    expect(acceptsTerminalInput(config(), runtime({ ptyAttached: false }))).toBe(false);
  });
});

describe("ptyChromeLabel", () => {
  it("reports the real attachment state, not the session type", () => {
    expect(
      ptyChromeLabel(config({ sessionType: "terminal" }), runtime({ ptyAttached: true })),
    ).toBe("ConPTY · interactive");
    expect(
      ptyChromeLabel(config({ sessionType: "terminal" }), runtime({ ptyAttached: false })),
    ).toBe("ConPTY · 未连接");
    expect(ptyChromeLabel(config(), runtime({ ptyAttached: true }))).toBe(
      "PTY attached · stdin 可用",
    );
    expect(ptyChromeLabel(config(), runtime({ ptyAttached: false }))).toBe("PTY 未连接 · 只读缓冲");
  });
});

describe("stoppedHint", () => {
  it("tells a terminal user the pane is not a log panel, and a service user what starting does", () => {
    expect(stoppedHint(fixture())).toBe("这个服务未运行。启动后它的输出会出现在这里。");
    expect(stoppedHint(fixture({ config: config({ sessionType: "terminal" }) }))).toBe(
      "交互终端必须先启动进程。这不是只读日志面板。",
    );
  });

  it("does not answer a service's question with the terminal's distinction", () => {
    // The overlay appears for a stopped session of either kind (observed on a
    // real service run, T11 #12). The terminal sentence names a distinction
    // — real PTY versus read-only log box — that only exists for terminals.
    expect(stoppedHint(fixture())).not.toContain("只读日志面板");
  });
});

describe("bufferDiscardNotice", () => {
  it("stays silent until output is actually lost, then counts the loss", () => {
    expect(bufferDiscardNotice(runtime())).toBeNull();
    expect(
      bufferDiscardNotice(runtime({ buffer: { bytes: 1, lines: 1, droppedBytes: 2048 } })),
    ).toBe("更早的输出已被丢弃（2048 B）");
  });
});

describe("runOutcomeBadge", () => {
  const live: RunRecordDto = {
    runId: "c8aa",
    sessionId: "comfyui",
    startedAt: "2026-09-28T05:00:00Z",
    logMode: "always",
    logSource: "captured",
  };

  /// The wire spells "this run is still going" as `endedAt: null` as often as
  /// as a missing key, and reading only the missing key files every live run
  /// under "error" (see the note in `types/runtime.ts`).
  it("reads both spellings of an unfinished run as running", () => {
    expect(runOutcome({ ...live, endedAt: null, exitCode: null })).toBe("running");
    expect(runOutcomeBadge({ ...live, endedAt: null, exitCode: null })).toEqual({
      label: "running",
      tone: "run",
    });
  });

  it("labels a run outcome with its tone", () => {
    expect(runOutcomeBadge(live)).toEqual({ label: "running", tone: "run" });
    expect(runOutcomeBadge({ ...live, endedAt: "2026-09-28T06:00:00Z", exitCode: 0 })).toEqual({
      label: "ok",
      tone: "idle",
    });
    expect(runOutcomeBadge({ ...live, endedAt: "2026-09-28T06:00:00Z", exitCode: 1 })).toEqual({
      label: "error",
      tone: "err",
    });
  });
});

describe("headerCallout", () => {
  it("shows the configured close impact verbatim while live", () => {
    const callout = headerCallout(config(), runtime());
    expect(callout).toEqual({
      kind: "impact",
      title: "关闭影响",
      text: "可停止；打开的聊天页会失联。",
    });
  });

  it("shows a terminal's configured close impact the same way (D-027)", () => {
    const callout = headerCallout(
      config({
        sessionType: "terminal",
        command: undefined,
        url: undefined,
        port: undefined,
        shell: "powershell",
        closeImpact: "仅结束本终端；不会停止其它受管服务。",
      }),
      runtime(),
    );
    expect(callout).toEqual({
      kind: "impact",
      title: "关闭影响",
      text: "仅结束本终端；不会停止其它受管服务。",
      note: "停止该终端会同时结束它启动的子进程。",
    });
  });

  it("renders an em dash when a service carries no close impact", () => {
    const callout = headerCallout(config({ closeImpact: undefined }), runtime());
    expect(callout).toEqual({ kind: "impact", title: "关闭影响", text: "—" });
  });

  it("gives a terminal without configured text the Hub's own sentence instead (#62)", () => {
    // A temporary terminal is created with no config text at all, and
    // `关闭影响 —` states nothing while looking like something failed to load.
    // The sentence that would have followed the dash is the whole card.
    const callout = headerCallout(
      config({ sessionType: "terminal", closeImpact: undefined, temporary: true }),
      runtime(),
    );
    expect(callout).toEqual({
      kind: "impact",
      title: "关闭影响",
      text: "停止该终端会同时结束它启动的子进程。",
    });
  });

  it("tells a live terminal what stopping it does to its process tree (D-028)", () => {
    // The configured text is kept, and the Hub's own consequence is added to
    // it rather than replacing it: `close_impact` is the user's sentence.
    const callout = headerCallout(
      config({
        sessionType: "terminal",
        shell: "powershell",
        closeImpact: "不会停止其它受管服务。",
      }),
      runtime(),
    );
    expect(callout?.text).toBe("不会停止其它受管服务。");
    expect(callout?.note).toContain("子进程");
  });

  it("notes nothing extra for a service, whose stop rules are its own", () => {
    expect(headerCallout(config(), runtime())).not.toHaveProperty("note");
  });
});

describe("closeMechanics", () => {
  it("names the graceful-then-force ladder for a service (D-007)", () => {
    const note = closeMechanics(config());
    expect(note).toContain("优雅结束");
    expect(note).toContain("强制结束");
  });

  it("tells a terminal what stopping it ends, with no ladder it does not have (D-018, D-028)", () => {
    // The sentence that used to stand here offered every session a gentle path.
    // A terminal has none — Ctrl+C is input, not a stop — so this says the one
    // thing that is true of closing it, the same fact the header warns with.
    const note = closeMechanics(config({ sessionType: "terminal", shell: "powershell" }));
    expect(note).toBe("停止该终端会同时结束它启动的子进程。");
    expect(note).not.toContain("优雅");
  });

  it("shows the last error instead once stopped", () => {
    const callout = headerCallout(
      config(),
      runtime({
        status: "stopped",
        lastError: { operation: "start", message: "端口 7788 已被占用" },
      }),
    );
    expect(callout).toEqual({ kind: "error", title: "上次错误", text: "端口 7788 已被占用" });
  });

  it("shows nothing for a clean stopped session", () => {
    expect(headerCallout(config(), runtime({ status: "stopped" }))).toBeNull();
  });
});

describe("logging labels", () => {
  it("labels modes and sources", () => {
    expect(logModeLabel("on_error")).toBe("On error");
    expect(logSourceLabel("captured")).toBe("Hub captured");
  });

  it("spells the metadata line's mode word lowercase", () => {
    expect(logModeToken("off")).toBe("off");
    expect(logModeToken("always")).toBe("always");
    expect(logModeToken("on_error")).toBe("on error");
    expect(logModeToken("manual")).toBe("manual");
  });

  it("answers 'is this being logged?' in one line", () => {
    expect(loggingHeadline({ mode: "off", source: "none" })).toBe("仅内存缓冲，不写磁盘");
    expect(loggingHeadline({ mode: "always", source: "captured" })).toBe(
      "每次运行都保存 stdout/stderr",
    );
    expect(loggingHeadline({ mode: "on_error", source: "captured" })).toBe("异常退出时才落盘");
    expect(loggingHeadline({ mode: "manual", source: "captured" })).toBe("需手动开始记录");
    expect(loggingHeadline({ mode: "off", source: "external", external_path: "D:/a.log" })).toBe(
      "关联应用日志，不重复捕获",
    );
  });
});

describe("metadataPairs", () => {
  it("renders pid, port, uptime, cwd and log value for a running service", () => {
    expect(metadataPairs(config(), runtime(), NOW)).toEqual([
      { label: "PID", value: "18420" },
      { label: "port", value: ":8000" },
      { label: "up", value: "3h 14m" },
      { label: "cwd", value: "D:/Tools/SillyTavern" },
      { label: "log", value: "always" },
    ]);
  });

  it("keeps the mode lowercase while the sidebar chip keeps its capital", () => {
    const session = fixture({
      config: config({ logging: { mode: "on_error", source: "captured" } }),
    });
    expect(metadataPairs(session.config, session.runtime, NOW)).toContainEqual({
      label: "log",
      value: "on error",
    });
    expect(sidebarRowMeta(session)).toContainEqual({ text: "On error" });
  });

  it("says buffer only for a terminal without persistence", () => {
    const terminal = config({
      sessionType: "terminal",
      url: undefined,
      port: undefined,
      shell: "pwsh",
    });
    const pairs = metadataPairs(
      terminal,
      runtime({
        sessionId: "pwsh",
        startedAt: "2026-09-28T08:02:25Z",
        logging: { mode: "off", source: "none", external_path: undefined },
      }),
      NOW,
    );
    expect(pairs).toEqual([
      { label: "PID", value: "18420" },
      { label: "up", value: "12m 5s" },
      { label: "cwd", value: "D:/Tools/SillyTavern" },
      { label: "log", value: "buffer only" },
    ]);
  });
});

describe("sidebarRowMeta", () => {
  it("describes services as port · svc, terminals as interactive · tty", () => {
    expect(sidebarRowMeta(fixture())).toEqual([
      { text: ":8000" },
      { text: "svc" },
      { text: "Always" },
    ]);
    expect(
      sidebarRowMeta(
        fixture({
          config: config({
            sessionType: "terminal",
            port: undefined,
            url: undefined,
            shell: "pwsh",
            logging: { mode: "off", source: "none" },
          }),
        }),
      ),
    ).toEqual([{ text: "interactive" }, { text: "tty" }]);
  });

  it("lets the live run override the config once one is running (LOGGING.md §1.4)", () => {
    expect(
      sidebarRowMeta(
        fixture({
          config: config({ logging: { mode: "off", source: "none" } }),
          runtime: runtime({
            logging: { mode: "manual", source: "captured", external_path: undefined },
          }),
        }),
      ),
    ).toContainEqual({ text: "Manual" });
  });

  it("reports an application-owned log as External when no mode is in effect", () => {
    expect(
      sidebarRowMeta(
        fixture({
          config: config({
            logging: { mode: "off", source: "external", externalPath: "D:/a.log" },
          }),
        }),
      ),
    ).toEqual([{ text: ":8000" }, { text: "svc" }, { text: "External" }]);
  });

  it("flags a busy running session and a failed one with a tone", () => {
    expect(sidebarRowMeta(fixture({ busy: true }))).toEqual([
      { text: ":8000" },
      { text: "svc" },
      { text: "busy", tone: "busy" },
      { text: "Always" },
    ]);
    expect(sidebarRowMeta(fixture({ runtime: runtime({ status: "error" }) }))).toContainEqual({
      text: "error",
      tone: "err",
    });
  });
});

describe("dependenciesOf", () => {
  it("resolves declared dependencies and ignores unknown ids", () => {
    const dep = fixture({ config: config({ id: "koboldcpp" }) });
    const subject = fixture({ dependsOn: ["koboldcpp", "missing"] });
    expect(dependenciesOf(subject, [subject, dep]).map((s) => s.config.id)).toEqual(["koboldcpp"]);
    expect(dependenciesOf(fixture(), [])).toEqual([]);
  });
});

describe("runOutcome", () => {
  it("derives the outcome from end and exit code", () => {
    const live: RunRecordDto = {
      runId: "c8aa",
      sessionId: "comfyui",
      startedAt: "2026-09-28T05:00:00Z",
      logMode: "always",
      logSource: "captured",
    };
    expect(runOutcome(live)).toBe("running");
    expect(runOutcome({ ...live, endedAt: "2026-09-28T06:00:00Z", exitCode: 0 })).toBe("ok");
    expect(runOutcome({ ...live, endedAt: "2026-09-28T06:00:00Z", exitCode: 1 })).toBe("error");
  });
});

describe("filterSessions / groupSessions", () => {
  const sessions = [
    fixture(),
    fixture({ config: config({ id: "comfyui", name: "ComfyUI", port: 8188 }), group: "ai" }),
    fixture({
      config: config({
        id: "test-api",
        name: "Test API",
        port: 7788,
        purpose: "本地调试用的临时 HTTP 接口。",
      }),
      group: "debug",
    }),
    fixture({
      config: config({
        id: "pwsh",
        name: "PowerShell",
        sessionType: "terminal",
        url: undefined,
        port: undefined,
        shell: "pwsh",
        purpose: "日常交互终端，跑一次性命令与 REPL。",
        closeImpact: "仅结束本终端；不会停止其它受管服务。",
        logging: { mode: "off", source: "none" },
      }),
      group: "debug",
    }),
  ];

  it("filters by name, port, purpose, id and type", () => {
    expect(filterSessions(sessions, "comfy").map((s) => s.config.id)).toEqual(["comfyui"]);
    expect(filterSessions(sessions, "8188").map((s) => s.config.id)).toEqual(["comfyui"]);
    expect(filterSessions(sessions, "调试").map((s) => s.config.id)).toEqual(["test-api"]);
    expect(filterSessions(sessions, "  SillyTavern ").map((s) => s.config.id)).toEqual([
      "sillytavern",
    ]);
    expect(filterSessions(sessions, "terminal").map((s) => s.config.id)).toEqual(["pwsh"]);
    // A terminal's `purpose` is searchable the same way a service's is
    // (D-027): the field is the session's own words, not a type's privilege.
    expect(filterSessions(sessions, "REPL").map((s) => s.config.id)).toEqual(["pwsh"]);
    expect(filterSessions(sessions, "").length).toBe(4);
  });

  it("groups by workload in declared order and drops empty groups", () => {
    const groups = groupSessions(sessions, FIXTURE_GROUPS);
    expect(groups.map((g) => g.group.label)).toEqual(["AI Apps", "Debug / Test"]);
    expect(groups[0].items.map((s) => s.config.id)).toEqual(["sillytavern", "comfyui"]);
    expect(groups[1].items.map((s) => s.config.id)).toEqual(["test-api", "pwsh"]);
  });
});

describe("initialSelectedSessionId", () => {
  const ids = ["sillytavern", "pwsh"];

  it("honours a matching #session deep link", () => {
    expect(initialSelectedSessionId("#session=pwsh", ids)).toBe("pwsh");
  });

  it("falls back to the first session for unknown, absent or malformed hashes", () => {
    expect(initialSelectedSessionId("#session=nope", ids)).toBe("sillytavern");
    expect(initialSelectedSessionId("", ids)).toBe("sillytavern");
    expect(initialSelectedSessionId(null, ids)).toBe("sillytavern");
    expect(initialSelectedSessionId("#session=bad id", ids)).toBe("sillytavern");
  });
});
