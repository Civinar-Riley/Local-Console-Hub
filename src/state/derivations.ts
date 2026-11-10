/**
 * Pure view derivations for the V2 workspace (T06 #7).
 *
 * Every rule the shell renders by — status tones and labels, action
 * availability, callout wording, metadata pairs, sidebar rows — lives here
 * as a pure function of the DTOs (plus the documented fixture extras), so
 * components stay thin and the rules are unit-tested without a DOM. The
 * wording mirrors the approved V2 prototype (docs/DESIGN_SPEC_EXTRACTED.md);
 * when T07–T10 replace fixtures with live snapshots, these functions keep
 * operating on the same shapes.
 */

import type { EffectiveLoggingDto, SessionConfigDto } from "../types/config";
import type {
  HttpProbeDto,
  RunRecordDto,
  RuntimeEffectiveLoggingDto,
  SessionRuntimeDto,
  SessionStatusValue,
} from "../types/runtime";
import { isPresent } from "../types/runtime";
import type { SessionView, WorkloadGroup } from "./session-view";

/** Pip/badge tone semantics (UI_STYLE_GUIDE §10). */
export type StatusTone = "run" | "busy" | "warn" | "err" | "idle";

/** Is the session mid-lifecycle (anything but fully stopped)? */
export function isLive(status: SessionStatusValue): boolean {
  return status === "running" || status === "starting" || status === "stopping";
}

/** Map lifecycle state (plus the busy runtime flag, spec §4) to a tone. */
export function statusTone(status: SessionStatusValue, busy = false): StatusTone {
  if (status === "error") return "err";
  if (status === "starting" || status === "stopping") return "warn";
  if (status === "running" && busy) return "busy";
  if (status === "running") return "run";
  return "idle";
}

/** Status badge / row-tail label; `Ready`/`Busy` supplement, never replace. */
export function statusLabel(status: SessionStatusValue, busy = false, ready = false): string {
  if (status === "running" && busy) return "Busy";
  if (status === "running" && ready) return "Ready";
  switch (status) {
    case "stopped":
      return "Stopped";
    case "starting":
      return "Starting";
    case "running":
      return "Running";
    case "stopping":
      return "Stopping";
    case "exited":
      return "Exited";
    case "error":
      return "Error";
  }
}

/** `Service` / `Terminal`. */
export function typeLabel(type: SessionConfigDto["sessionType"]): string {
  return type === "service" ? "Service" : "Terminal";
}

/**
 * Whether a session is up and usable, rather than merely held by the lifecycle
 * (T08 #9, spec §4's optional `ready` flag).
 *
 * `Running` says the managed process or shell is there; this says the thing the
 * user actually wants from it is there too — a service answering on its port,
 * or a terminal whose shell is attached and can take typing. The two are kept
 * apart on purpose: `docs/PRODUCT_SPEC.md` §3 requires the UI to distinguish
 * "the process is alive" from "the service is available", and D-008 forbids
 * collapsing one into the other.
 *
 * Both halves are read from what the backend reported, never inferred. A
 * service nobody has probed yet shows `Running` — the state machine knows that
 * much and nothing more — and a service with no port to check is never called
 * ready at all, since there is nothing that could say it was.
 */
export function isReady(runtime: SessionRuntimeDto): boolean {
  if (runtime.status !== "running") {
    return false;
  }
  // A reading is the service's answer; without one (a terminal run, or a
  // service with no port) attachment is the only readiness there is to report.
  if (isPresent(runtime.health)) {
    // Both facts, because either one alone is a claim the reading does not
    // support: a port answering while *this* run's process is gone is something
    // else listening on it, which is not this session being ready.
    return runtime.health.processAlive && runtime.health.portOpen;
  }
  return runtime.ptyAttached;
}

/**
 * The Details tab's one-line health reading, or `undefined` when there is none.
 *
 * TCP reachability and the process are worded separately, because the product
 * separates them: a service that is up, one that is still booting, and one
 * whose process has gone must not read the same. When an HTTP probe was
 * issued, its result is a further clause on the same line. A 2xx status is
 * reported as that status; a timeout or a non-success status is a failed
 * probe. That clause does not replace "未监听" or "本会话进程已退出".
 *
 * A session nothing has probed gets no line at all — the row disappears
 * rather than claiming a port is closed (spec §12: "we did not check" is not
 * a reading). A session with no http(s) URL has no HTTP clause, and the TCP
 * wording is unchanged.
 *
 * This is the only place a reading is shown, and it names the *port* only by
 * implication: the number is already in the header's metadata line, and §6 says
 * Details carries the low-frequency fields only, "values already live in the
 * header metadata line — PID, port, uptime, cwd, effective log policy — are not
 * repeated here". The header's status badge states the conclusion the TCP
 * reading earns (`Ready`); the HTTP clause does not change that badge.
 */
export function healthReading(runtime: SessionRuntimeDto): string | undefined {
  const health = runtime.health;
  if (!isPresent(health)) return undefined;
  const listening = health.portOpen ? "监听中" : "未监听";
  const tcp = health.processAlive ? listening : `${listening} · 本会话进程已退出`;
  const http = httpClause(health.http);
  return http === undefined ? tcp : `${tcp} · ${http}`;
}

/** The HTTP half of a health line, absent when that probe was not issued. */
function httpClause(http: HttpProbeDto | null | undefined): string | undefined {
  if (!isPresent(http)) return undefined;
  if (http.ok) {
    return typeof http.status === "number" ? `HTTP ${http.status}` : "HTTP 正常";
  }
  return typeof http.status === "number" ? `HTTP ${http.status} 探测失败` : "HTTP 探测失败";
}

/** Elapsed run duration in the reference's vocabulary. */
export function formatDuration(startedAt: string, now: Date): string {
  const started = Date.parse(startedAt);
  if (Number.isNaN(started)) {
    return "—";
  }
  const sec = Math.max(0, Math.floor((now.getTime() - started) / 1000));
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

/** App-wide counts: running, busy (running + busy flag) and total. */
export interface LiveCounts {
  total: number;
  running: number;
  busy: number;
}

/** Count the live summary over sessions carrying runtime + busy flag. */
export function liveCounts(sessions: SessionView[]): LiveCounts {
  let running = 0;
  let busy = 0;
  for (const session of sessions) {
    if (session.runtime.status === "running") {
      running += 1;
      if (session.busy) busy += 1;
    }
  }
  return { total: sessions.length, running, busy };
}

/** Sidebar-header summary: `2 运行 · 1 忙碌 · 6 会话`. */
export function sidebarSummaryText(counts: LiveCounts): string {
  const busy = counts.busy ? ` · ${counts.busy} 忙碌` : "";
  return `${counts.running} 运行${busy} · ${counts.total} 会话`;
}

/** Title-bar summary: `2 运行 · 1 busy`. */
export function titlebarSummaryText(counts: LiveCounts): string {
  const busy = counts.busy ? ` · ${counts.busy} busy` : "";
  return `${counts.running} 运行${busy}`;
}

/**
 * Which header actions exist, derived from lifecycle truth (spec §5).
 *
 * Components render this verbatim — the lifecycle rules live here, once, so
 * a button's enablement cannot drift from the state machine.
 */
export interface ActionAvailability {
  /** Start is offered while idle; Stop while live. Never both at once. */
  start: boolean;
  stop: boolean;
  /**
   * Whether the Hub owns this session's lifecycle (#66).
   *
   * `false` means the run belongs to the application itself: `stop`,
   * `forceStop` and `restart` are all withheld, because Session Core refuses
   * them rather than ignoring them. What the header shows instead is the one
   * action that does work — opening the application, which brings its window
   * forward.
   */
  managed: boolean;
  /**
   * Whether the run in flight is one the Hub did not start (#67).
   *
   * Reported beside `managed` rather than folded into it because the two say
   * different things and the header has to explain the right one: an entry may
   * be configured for Hub management *and* be associated with an instance the
   * user was already running, and then "enable management in the config" is
   * advice about a setting that is already on.
   */
  associated: boolean;
  /** Stop stays visible but inert while the session is already unwinding. */
  stopDisabled: boolean;
  /** Restart cannot launch a replacement until the previous run is gone
   * (spec §5 rule 2), so it is offered only from a settled state. */
  restart: boolean;
  /** Open the configured URL (service with a URL). */
  openUrl?: string;
  /** Open the working directory (`目录`, and the menu's `打开目录`) — offered
   * only when there is one to open. Both "open" actions are gated on the config
   * carrying a target for them, so the button cannot be pressed for something
   * the backend would refuse (T08 #9). */
  directory: boolean;
  /** Put that same directory on the clipboard (the menu's `复制路径`).
   *
   * Gated exactly like `directory`, and for the same reason: the controls act
   * on the session's working directory, so a session without one has nothing to
   * open *and* nothing to copy. A control that could only answer "there is no
   * cwd" is not worth offering (UI_STYLE_GUIDE §5 lists it among the header's
   * context actions, which exist only where the context does). */
  copyPath: boolean;
  /** Force-kill the managed tree — a separate, explicit action (D-007).
   * Available for the whole live window, including Stopping: skipping the
   * grace period is exactly what this action is for. */
  forceStop: boolean;
  /**
   * Save this terminal's launch configuration (#65).
   *
   * A temporary terminal's own action, and the only one of its two that is
   * offered while the terminal is *running*: saving is what a user does when
   * the shell and directory in front of them turn out to be worth keeping, and
   * asking them to end the terminal first would be asking them to give up the
   * thing they are saving. It stays on offer after the run ends too — an ended
   * terminal is still a launch method — and it is gone the moment the session
   * is saved, because `temporary` is exactly the "not saved yet" flag.
   *
   * The backend's own gate is the same flag (`app::terminals`, and the core's
   * `mark_saved` refuses a configured session), so this cannot offer a control
   * that would only be refused.
   */
  saveConfig: boolean;
  /**
   * Edit this application's saved launch configuration.
   *
   * Configured service entries only. A temporary terminal has no file entry —
   * its "保存启动配置" is what writes one — and a session with a live run was
   * started from the configuration being edited, so Session Core refuses both;
   * the control is absent rather than offered-and-refused.
   *
   * `error` stays editable: a failed start left a configuration with nothing
   * running. A failed stop that still owns its tree is the one case where the
   * backend refuses an edit the header offered, because the run's liveness is
   * not something this snapshot reports.
   */
  edit: boolean;
  /**
   * Remove this session from the list (#62).
   *
   * A temporary terminal's own action, offered once its run has ended: that is
   * when there is nothing left to own and the row is only holding scrollback
   * (story 22). A configured session never gets it — it lives in the config
   * file, and Session Core refuses the removal anyway.
   *
   * "Ended" is `stopped` or `exited` and not merely "not live", which is the
   * distinction `error` makes: a stop that could not confirm the terminal's
   * process tree was gone reports `error` **while still owning that tree**, and
   * the control that would drop the last handle accounting for it must not be
   * offered (#62: "使用 #61 的安全结束能力，不绕过归属"). The backend's own
   * gate is `session::core::removable`, and this mirrors it.
   */
  remove: boolean;
}

/** Derive the header action set for the current lifecycle state. */
export function availableActions(
  config: SessionConfigDto,
  runtime: SessionRuntimeDto,
): ActionAvailability {
  const idle = !isLive(runtime.status);
  const settled = runtime.status === "running" || idle;
  // The path controls read one thing — the config's working directory — so it
  // is asked once: 目录/打开目录 and 复制路径 cannot end up disagreeing about
  // whether there is one.
  const hasDirectory = config.cwd !== undefined;
  // A run the Hub does not own cannot be stopped or restarted by it (#66), and
  // Session Core refuses both rather than quietly ignoring them — so the
  // controls are absent for the same reason `remove` is absent for a configured
  // session. Opening it stays: starting an application it manages to *launch*
  // is what the entry is for (spec #59 decision 12).
  //
  // An associated instance is the same answer from the other direction (#67):
  // the Hub *did* start nothing here, whatever the configuration says about
  // lifecycle — that setting is a statement about the Hub's own runs — so the
  // controls are absent for the run in flight. A button whose only outcome is
  // a refusal is the "看起来能用的按钮" this header's rules exist to avoid.
  const managed = config.lifecycle === "managed";
  const owned = managed && !runtime.external;
  return {
    start: idle,
    stop: owned && isLive(runtime.status),
    managed,
    associated: runtime.external,
    stopDisabled: runtime.status === "stopping",
    restart: owned && settled,
    openUrl: config.sessionType === "service" ? config.url : undefined,
    directory: hasDirectory,
    copyPath: hasDirectory,
    forceStop: owned && isLive(runtime.status),
    saveConfig: config.temporary === true,
    edit: config.temporary !== true && config.sessionType === "service" && idle,
    remove:
      config.temporary === true && (runtime.status === "stopped" || runtime.status === "exited"),
  };
}

/**
 * Whether this session is displayed in its own window rather than the Hub's
 * (#66).
 *
 * The one predicate the surfaces that would otherwise draw a terminal or a log
 * panel ask first: an application that keeps its own console has nothing for
 * the Hub to render, and showing an empty terminal for it would be exactly the
 * "假内嵌" spec #59 decision 16 forbids.
 */
export function isStandalone(config: SessionConfigDto): boolean {
  return config.display === "window";
}

/** What the standalone application's panel says about where its console is. */
export function standaloneNote(runtime: SessionRuntimeDto): string {
  if (isLive(runtime.status)) {
    return "此应用使用独立窗口显示：它的窗口和控制台由应用自己提供，Hub 不重复内嵌。";
  }
  return "此应用使用独立窗口显示：启动后它会在自己的窗口里运行，Hub 不显示它的控制台。";
}

/**
 * The terminal panel's connection strip.
 *
 * Reads `ptyAttached` rather than the session type: UI_STYLE_GUIDE §7 calls
 * the terminal "a real PTY view, not a read-only log box", so the surface
 * must not claim an attachment the snapshot does not report.
 */
export function ptyChromeLabel(config: SessionConfigDto, runtime: SessionRuntimeDto): string {
  if (!runtime.ptyAttached) {
    return config.sessionType === "terminal" ? "ConPTY · 未连接" : "PTY 未连接 · 只读缓冲";
  }
  return config.sessionType === "terminal" ? "ConPTY · interactive" : "PTY attached · stdin 可用";
}

/**
 * What the terminal pane's "not running" overlay says a start would get you.
 *
 * The reason to start a session is not the same for both kinds, so neither is
 * the sentence. A stopped interactive terminal is a PTY nobody can type into,
 * and the overlay's job there is to say it is not a log panel standing in for
 * one (UI_STYLE_GUIDE §7). A stopped service is simply not writing anything
 * yet; telling that user "this is not a read-only log panel" answers a
 * question they did not ask, about a distinction that does not apply to them.
 */
export function stoppedHint(session: SessionView): string {
  return session.config.sessionType === "terminal"
    ? "交互终端必须先启动进程。这不是只读日志面板。"
    : "这个服务未运行。启动后它的输出会出现在这里。";
}

/**
 * Whether a session's terminal view may send keystrokes (T07 #8).
 *
 * The backend's own predicate at `terminal_write`, transcribed rather than
 * approximated: it accepts input only for a run that is a terminal *and*
 * `Running` (`SessionCore::terminal_write`'s `Input::Pty` arm), so this asks
 * for exactly those three things over the DTOs — the type, the lifecycle state
 * and the attachment.
 *
 * The state conjunct is not redundant with `ptyAttached`, which is the subtle
 * part: the flag is cleared when the run *ends* (`close_run`), while `stop`
 * sets `Stopping` first and then waits out the grace period. Without the state
 * check a pane is typable throughout that window — and every keystroke comes
 * back as "has no running terminal; start it before typing into it", which is
 * the per-keystroke refusal notice this gate exists to prevent. The same
 * window does not exist on the way up: `start` sets `Running` and the flag in
 * one lock hold, so no snapshot reports the attachment before the state.
 *
 * A supervised service needs no separate case: it "has no attached stdin to
 * type into", which `terminal_write` refuses with its own test.
 */
export function acceptsTerminalInput(
  config: SessionConfigDto,
  runtime: SessionRuntimeDto,
): boolean {
  return config.sessionType === "terminal" && runtime.status === "running" && runtime.ptyAttached;
}

/**
 * Scrollback-loss notice, or null when nothing was discarded
 * (`docs/LOGGING.md` §8: discarded output is counted and surfaced so the UI
 * can say "older output was dropped" instead of showing a gapped history).
 */
export function bufferDiscardNotice(runtime: SessionRuntimeDto): string | null {
  if (runtime.buffer.droppedBytes <= 0) {
    return null;
  }
  return `更早的输出已被丢弃（${runtime.buffer.droppedBytes} B）`;
}

/** Sessions this one depends on, resolved from the workspace it is rendered in. */
export function dependenciesOf(session: SessionView, all: readonly SessionView[]): SessionView[] {
  return (session.dependsOn ?? [])
    .map((id) => all.find((candidate) => candidate.config.id === id))
    .filter((candidate): candidate is SessionView => candidate !== undefined);
}

/** Badge label + tone for a run record's outcome. */
export function runOutcomeBadge(run: RunRecordDto): { label: string; tone: StatusTone } {
  const outcome = runOutcome(run);
  return {
    label: outcome,
    tone: outcome === "running" ? "run" : outcome === "error" ? "err" : "idle",
  };
}

/** The header callout: close impact while live, last error otherwise. */
export interface HeaderCallout {
  kind: "impact" | "error";
  title: string;
  text: string;
  /**
   * A consequence the Hub knows from its own lifecycle, shown under the
   * configured text rather than in place of it. `close_impact` is the user's
   * own wording (D-027); this is the part no config author can be expected to
   * write.
   */
  note?: string;
}

/**
 * What the Hub itself does when an interactive terminal is stopped: the shell
 * *and* the processes it started end (D-028).
 *
 * A terminal's `close_impact` is free text about the session's place in the
 * user's world ("不会停止其它受管服务") and says nothing about this. A service
 * deliberately gets no note: its close impact arrives with its own stop rules,
 * and writing a sentence for it here would claim more than the Hub does there.
 */
const TERMINAL_TREE_NOTE = "停止该终端会同时结束它启动的子进程。";

/**
 * What the Details card says about the *mechanics* of stopping this session.
 *
 * A service has the graceful-then-force ladder (D-007), so its sentence names
 * both steps and where the force path stops. A terminal has no ladder at all:
 * its graceful gesture is Ctrl+C, which is *input*, not a stop (D-018), and
 * stopping it ends its process tree in one action (D-028). The card therefore
 * says the same sentence the header warns with for a terminal — it is the same
 * fact, and the sentence that used to sit here told a terminal's user about a
 * gentle path the Hub does not offer it.
 */
export function closeMechanics(config: SessionConfigDto): string {
  return config.sessionType === "terminal"
    ? TERMINAL_TREE_NOTE
    : "停止会先尝试优雅结束，超时后强制结束本会话的受管进程树。";
}

/** Pick and word the header callout for the current state. */
export function headerCallout(
  config: SessionConfigDto,
  runtime: SessionRuntimeDto,
): HeaderCallout | null {
  if (isLive(runtime.status)) {
    if (config.sessionType === "terminal") {
      // A terminal whose config carries no close impact still has one thing to
      // say, and it is the Hub's own sentence (D-028): a temporary terminal is
      // created without any config text at all, and `关闭影响 —` states
      // nothing while looking like something failed to load. So the sentence
      // moves up into the text rather than following a dash.
      return config.closeImpact === undefined
        ? { kind: "impact", title: "关闭影响", text: TERMINAL_TREE_NOTE }
        : {
            kind: "impact",
            title: "关闭影响",
            text: config.closeImpact,
            note: TERMINAL_TREE_NOTE,
          };
    }
    return { kind: "impact", title: "关闭影响", text: config.closeImpact ?? "—" };
  }
  if (isPresent(runtime.lastError)) {
    return { kind: "error", title: "上次错误", text: runtime.lastError.message };
  }
  return null;
}

/** Human label of an effective log mode. */
export function logModeLabel(mode: RuntimeEffectiveLoggingDto["mode"]): string {
  switch (mode) {
    case "off":
      return "Off";
    case "always":
      return "Always";
    case "on_error":
      return "On error";
    case "manual":
      return "Manual";
  }
}

/**
 * The mode word as the header metadata line spells it: lowercase.
 *
 * The reference spells the same mode two ways on purpose, and both are visible
 * in it: the metadata line is a policy *token* strip (`log always`, `log off`,
 * `log buffer only`), while the sidebar chip and the Details/Logs surfaces
 * spell it as a sentence-case label (`Always`, `Off`). So this is a second
 * spelling of one vocabulary, not a second vocabulary — lowercasing
 * `logModeLabel` at the call site would drag the chip's capital down with it,
 * and capitalising this one would put a label where the reference has a token.
 *
 * The one place it departs from the reference's letter is `on_error`: the
 * prototype renders the raw enum there, underscore and all, which would leak a
 * wire value into the strip. Recorded in DESIGN_SPEC_EXTRACTED §5.
 */
export function logModeToken(mode: RuntimeEffectiveLoggingDto["mode"]): string {
  return logModeLabel(mode).toLowerCase();
}

/** Human label of a log source. */
export function logSourceLabel(source: RuntimeEffectiveLoggingDto["source"]): string {
  switch (source) {
    case "none":
      return "None";
    case "captured":
      return "Hub captured";
    case "external":
      return "External";
  }
}

/** One-line answer to "is this being logged?" (Logs pane headline). */
export function loggingHeadline(logging: RuntimeEffectiveLoggingDto | EffectiveLoggingDto): string {
  if (logging.source === "external") return "关联应用日志，不重复捕获";
  if (logging.mode === "off") return "仅内存缓冲，不写磁盘";
  if (logging.mode === "always") return "每次运行都保存 stdout/stderr";
  if (logging.mode === "on_error") return "异常退出时才落盘";
  return "需手动开始记录";
}

/** Compact header metadata: label/value pairs in the reference's shape. */
export function metadataPairs(
  config: SessionConfigDto,
  runtime: SessionRuntimeDto,
  now: Date,
): Array<{ label: string; value: string }> {
  const logging = runtime.logging ?? config.logging;
  const logValue =
    logging.source === "none"
      ? "buffer only"
      : logging.source === "external"
        ? "external"
        : logModeToken(logging.mode);
  const pairs: Array<{ label: string; value: string }> = [
    { label: "PID", value: isPresent(runtime.pid) ? String(runtime.pid) : "—" },
  ];
  if (config.port !== undefined) pairs.push({ label: "port", value: `:${config.port}` });
  if (isPresent(runtime.startedAt) && runtime.status === "running") {
    pairs.push({ label: "up", value: formatDuration(runtime.startedAt, now) });
  }
  pairs.push({ label: "cwd", value: config.cwd ?? "—" });
  pairs.push({ label: "log", value: logValue });
  return pairs;
}

/** One chip in a sidebar row's meta line. */
export interface RowMetaChip {
  text: string;
  tone?: "busy" | "err";
}

/** Sidebar row meta: `:8000 · svc` or `interactive · tty`, plus flags.
 *
 * The logging chip reports the *effective* policy: the mode when one is in
 * effect, otherwise `External` for an application-owned log. The prototype's
 * `Auto` label cannot appear here — `auto` resolves before the frontend
 * (src/types/config.ts), so the row states what is actually happening. */
export function sidebarRowMeta(session: SessionView): RowMetaChip[] {
  const chips: RowMetaChip[] = [];
  if (session.config.sessionType === "service") {
    chips.push({ text: session.config.port !== undefined ? `:${session.config.port}` : "service" });
    chips.push({ text: "svc" });
  } else {
    chips.push({ text: "interactive" });
    chips.push({ text: "tty" });
  }
  if (session.runtime.status === "running" && session.busy) {
    chips.push({ text: "busy", tone: "busy" });
  }
  if (session.runtime.status === "error") {
    chips.push({ text: "error", tone: "err" });
  }
  // Live state wins once a run exists (LOGGING.md §1.4): a `manual` run that
  // has started recording must show as such in the rail, not as its config.
  const logging = session.runtime.logging ?? session.config.logging;
  if (logging.mode !== "off") {
    chips.push({ text: logModeLabel(logging.mode) });
  } else if (logging.source === "external") {
    chips.push({ text: "External" });
  }
  return chips;
}

/** Outcome of a run record for the history list. */
export function runOutcome(run: RunRecordDto): "running" | "ok" | "error" {
  if (!isPresent(run.endedAt)) return "running";
  return run.exitCode === 0 ? "ok" : "error";
}

/** A sidebar workload group. */
export interface SessionGroup {
  group: WorkloadGroup;
  items: SessionView[];
}

/** Group sessions by workload in the fixture groups' declared order. */
export function groupSessions(
  sessions: SessionView[],
  groups: readonly WorkloadGroup[],
): SessionGroup[] {
  return groups
    .map((group) => ({
      group,
      items: sessions.filter((session) => session.group === group.id),
    }))
    .filter((entry) => entry.items.length > 0);
}

/** Filter sessions by name, purpose, port, id or type, case-insensitively. */
export function filterSessions(sessions: SessionView[], rawQuery: string): SessionView[] {
  const query = rawQuery.trim().toLowerCase();
  if (!query) return sessions;
  return sessions.filter((session) => {
    const { config } = session;
    return (
      config.name.toLowerCase().includes(query) ||
      (config.purpose?.toLowerCase().includes(query) ?? false) ||
      (config.port !== undefined && String(config.port).includes(query)) ||
      config.id.toLowerCase().includes(query) ||
      config.sessionType.includes(query)
    );
  });
}

/**
 * Resolve the initially selected session from a `#session=<id>` deep link,
 * falling back to the first known session. Unknown ids fall back rather than
 * render an empty workspace; `null` hash input keeps this testable.
 */
export function initialSelectedSessionId(hash: string | null, ids: readonly string[]): string {
  const match = /^#session=([A-Za-z0-9_-]+)$/.exec(hash ?? "");
  const wanted = match?.[1];
  if (wanted !== undefined && ids.includes(wanted)) {
    return wanted;
  }
  return ids[0] ?? "";
}
