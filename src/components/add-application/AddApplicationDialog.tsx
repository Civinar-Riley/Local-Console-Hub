import { useEffect, useRef, useState } from "react";
import { FolderOpen, FileSearch, X } from "lucide-react";
import {
  applicationUrl,
  selectedProgram,
  type DirectoryScan,
  type LaunchCandidate,
  type PathKind,
} from "../../types/discovery";
import type {
  DisplayAdviceDto,
  DisplayModeValue,
  FormErrorDto,
  FormSaveOutcome,
  NewApplicationFormDto,
} from "../../types/config";
import "../dialog/dialog.css";

/**
 * How long the form waits after a keystroke before asking the backend about the
 * launch method (#66).
 *
 * The answer is a filesystem lookup (which executable the command resolves to,
 * and which subsystem it records), so asking per keystroke would be one IPC
 * round trip per character for an answer that only changes when the command
 * settles.
 */
const RECOMMEND_DEBOUNCE_MS = 250;

export interface AddApplicationDialogProps {
  /**
   * Save the form. The dialog stays open while this is in flight and reports
   * what came back — including a refusal, which is why it never assumes the
   * save succeeded (spec #59 decision 15: 只在持久保存确认后反馈成功).
   */
  onSubmit: (form: NewApplicationFormDto) => Promise<FormSaveOutcome>;
  /**
   * Ask what the Hub can confirm about a launch method's display mode (#66).
   *
   * Injected like every other backend call in this component, so the dialog
   * stays renderable — and testable — with no backend at all. Absent means the
   * form offers both modes without a recommendation.
   */
  onRecommendDisplay?: (command: string, cwd: string) => Promise<DisplayAdviceDto | null>;
  onPickPath?: (kind: PathKind, cwd: string) => Promise<string | null>;
  onScanDirectory?: (cwd: string) => Promise<DirectoryScan>;
  onClose: () => void;
}

/**
 * The config-layer field names this form has an input for.
 *
 * The backend's refusals name the *config* field (`close_impact`, not
 * `closeImpact`), which is what lets the two sides share one vocabulary
 * without the form re-mapping anything.
 */
const RENDERED_FIELDS: ReadonlySet<string> = new Set([
  "name",
  "cwd",
  "command",
  "port",
  "url",
  "purpose",
  "close_impact",
  "lifecycle",
  "logging.path",
  "logging.source",
]);

/**
 * Where a refusal belongs, or `null` for "in the banner".
 *
 * A refusal is shown beside its input only when that input is on screen:
 * `logging.path` exists only for the external policy, and a field this form
 * does not render — a future one, or one the config layer adds — has nowhere
 * to go. Both fall through to the banner rather than disappearing, which is
 * the failure mode worth designing out (story 31: 保存失败有明确反馈).
 */
export function errorPlacement(error: FormErrorDto | null, policy: string): string | null {
  const field = error?.field;
  if (field === undefined || !RENDERED_FIELDS.has(field)) return null;
  if (field === "logging.path" && policy !== "external") return null;
  return field;
}

/**
 * The logging policies a display mode can be given (#66).
 *
 * A standalone-window application keeps its own console, so the config layer
 * refuses `source: captured` for it (spec #59 decision 16) — and offering a
 * policy the save would reject is the "按下去不工作" option decision 8 rules
 * out. What remains is the two that mean something for such an entry: record
 * nothing, or link the log the application writes itself.
 */
export function logPoliciesFor(
  display: DisplayModeValue,
): ReadonlyArray<{ value: string; label: string; hint: string }> {
  if (display === "internal") return LOG_POLICIES;
  return LOG_POLICIES.filter(
    (option) => option.value === "" || option.value === "off" || option.value === "external",
  ).map((option) =>
    // The unspecified policy's label names the default it falls back to, and
    // that default is the *other* one for this mode: a standalone entry
    // captures nothing (spec #59 decision 16), so leaving the box alone must
    // not read as "出错时记录".
    option.value === ""
      ? {
          ...option,
          label: "未指定（独立窗口：不捕获输出）",
          hint: "不写 `logging:` 块：应用自己的控制台不经过 Hub",
        }
      : option,
  );
}

/**
 * The logging policies the form offers.
 *
 * Each one is a combination the config layer already accepts, stated in its
 * own vocabulary (`docs/LOGGING.md` §3) — the dialog does not invent a policy,
 * it picks between the ones the file format has. The empty value is "not
 * specified", which is what lets the config layer's defaults apply.
 */
const LOG_POLICIES: ReadonlyArray<{ value: string; label: string; hint: string }> = [
  { value: "", label: "未指定（服务默认：出错时记录）", hint: "不写 `logging:` 块，沿用默认" },
  { value: "off", label: "关闭", hint: "只保留内存滚动缓冲，不落盘" },
  { value: "on_error", label: "出错时记录", hint: "运行失败时写一份 Hub 日志" },
  { value: "always", label: "始终记录", hint: "运行期间持续写入 Hub 日志" },
  { value: "manual", label: "仅手动记录", hint: "只有手动保存时才写日志" },
  { value: "external", label: "关联应用自有日志", hint: "Hub 只指向应用自己写的日志文件" },
];

/**
 * The two display modes the form offers (#66).
 *
 * Both are real: "Hub 内显示" hosts the command on a console the Hub owns, and
 * "独立窗口" keeps the application's own window and console. Neither is a
 * placeholder for the other, which is the difference between this control and
 * the sentence that stood here before the mode existed (spec #59 decision 8).
 */
const DISPLAY_OPTIONS: ReadonlyArray<{ value: DisplayModeValue; label: string }> = [
  { value: "internal", label: "Hub 内显示" },
  { value: "window", label: "独立窗口" },
];

/**
 * What the form says under the display choice.
 *
 * Two things, in this order: what the *choice* means, and — when the backend
 * could confirm something about this command — what it confirmed and why. The
 * recommendation sentence is the backend's own, because the fact it names (which
 * subsystem the executable records) is not something this component knows.
 */
export function displayHint(display: DisplayModeValue, advice: DisplayAdviceDto | null): string {
  const meaning =
    display === "window"
      ? "启动后在应用自己的窗口里运行，Hub 不重复内嵌它的控制台。"
      : "在 Hub 窗口里用终端显示它的输出。";
  if (advice === null) return meaning;
  // The reason is the backend's own sentence: it names what it looked at, which
  // is what makes the recommendation checkable rather than a claim (spec #59
  // decision 9).
  return `${meaning}${advice.recommended === display ? "Hub 推荐这一项：" : ""}${advice.reason}`;
}

/** The logging block for a chosen policy, or `undefined` for "not specified". */
export function loggingFor(
  policy: string,
  path: string,
): NewApplicationFormDto["logging"] | undefined {
  switch (policy) {
    case "off":
      return { mode: "off", source: "none" };
    case "on_error":
      return { mode: "on_error", source: "captured" };
    case "always":
      return { mode: "always", source: "captured" };
    case "manual":
      return { mode: "manual", source: "captured" };
    case "external":
      return { mode: "always", source: "external", path: path.trim() };
    default:
      return undefined;
  }
}

/**
 * The logging policy a display mode can carry, given the one in hand.
 *
 * A policy that captures Hub-side output cannot survive a switch to a mode
 * with no Hub console: the config layer refuses that combination, so the form
 * must not be left holding it (spec #59 decision 8 — an option that would be
 * rejected is not an option). Switching back to `internal` keeps whatever is
 * selected, because every policy is legal there.
 */
export function legalPolicyFor(mode: DisplayModeValue, policy: string): string {
  const legal = logPoliciesFor(mode);
  return legal.some((option) => option.value === policy) ? policy : legal[0].value;
}

/**
 * The "添加应用" form (#64, spec #59 decision 7).
 *
 * The secondary entry, deliberately apart from "新建 PowerShell": that one
 * creates a terminal immediately and asks nothing, this one saves a launch
 * configuration the Hub will list and reuse. Name, working directory and
 * command are required; purpose, close impact, port, web address and the log
 * policy are optional, and a blank optional box is left out of the file rather
 * than written as an empty value.
 *
 * ## The two dimensions this form also asks about
 *
 * 展示方式 and 生命周期归属 (#66) are separate settings, and the form says so:
 * where the application is displayed, and — only for an entry that keeps its own
 * window — whether the Hub ends it along with everything else. The display
 * choice starts on what the Hub could *confirm* about the launch method
 * (spec #59 decision 9), and stays wherever the user puts it.
 */
export default function AddApplicationDialog({
  onSubmit,
  onRecommendDisplay,
  onPickPath,
  onScanDirectory,
  onClose,
}: AddApplicationDialogProps) {
  const [name, setName] = useState("");
  const [cwd, setCwd] = useState("");
  const [command, setCommand] = useState("");
  const [purpose, setPurpose] = useState("");
  const [closeImpact, setCloseImpact] = useState("");
  const [port, setPort] = useState("");
  const [url, setUrl] = useState("");
  const [protocol, setProtocol] = useState("http://");
  const [scan, setScan] = useState<DirectoryScan | null>(null);
  const [picking, setPicking] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const dirty = useRef(new Set<string>());
  const request = useRef(0);
  useEffect(
    () => () => {
      request.current += 1;
    },
    [],
  );
  const [policy, setPolicy] = useState(LOG_POLICIES[0].value);
  const [logPath, setLogPath] = useState("");
  const [display, setDisplay] = useState<DisplayModeValue>("internal");
  /** Whether the user has chosen a mode, which is what makes it theirs. */
  const [displayTouched, setDisplayTouched] = useState(false);
  const [manageLifecycle, setManageLifecycle] = useState(false);
  const [advice, setAdvice] = useState<DisplayAdviceDto | null>(null);
  /**
   * The command the current advice is about.
   *
   * Advice is derived from a command, so it has to travel with the one it was
   * asked about: while the user is editing, the previous answer is about a
   * launch method that is no longer on screen, and showing it would attribute
   * one command's console to another.
   */
  const [advisedFor, setAdvisedFor] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<FormErrorDto | null>(null);

  useEffect(() => {
    if (!dirty.current.has("url")) {
      const number = Number(port);
      setUrl(
        Number.isInteger(number) && number > 0 && number <= 65535 ? `127.0.0.1:${number}` : "",
      );
    }
  }, [port]);

  const applyCandidate = (candidate: LaunchCandidate, explicit = false) => {
    if (explicit || !dirty.current.has("command")) {
      setCommand(candidate.command);
      setCwd(candidate.cwd);
    }
    if (!dirty.current.has("port")) setPort(candidate.port === null ? "" : String(candidate.port));
  };

  const discover = async (directory: string, generation: number) => {
    if (onScanDirectory === undefined) return;
    const result = await onScanDirectory(directory);
    if (generation !== request.current) return;
    setScan(result);
    if (!dirty.current.has("name")) setName(result.name);
    if (result.candidates.length === 1) applyCandidate(result.candidates[0]);
    if (!dirty.current.has("log") && result.logs.length === 1) setLogPath(result.logs[0]);
  };

  const choosePath = async (kind: PathKind) => {
    if (!onPickPath || picking) return;
    const generation = ++request.current;
    setPicking(true);
    setScanError(null);
    try {
      const path = await onPickPath(kind, cwd);
      if (generation !== request.current || path === null) return;
      if (kind === "log") {
        setLogPath(path);
        dirty.current.add("log");
      } else if (kind === "program") {
        const candidate = selectedProgram(path);
        applyCandidate(candidate, true);
        dirty.current.add("command");
        if (!dirty.current.has("name"))
          setName(
            path
              .split(/[\\/]/)
              .at(-1)
              ?.replace(/\.[^.]+$/, "") ?? "",
          );
        setScan(null);
      } else {
        setCwd(path);
        setScan(null);
        await discover(path, generation);
      }
    } catch (error) {
      if (generation === request.current)
        setScanError(error instanceof Error ? error.message : String(error));
    } finally {
      if (generation === request.current) setPicking(false);
    }
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      // A save in flight is the one moment the form may not go away: its
      // refusal is answered on this component, and closing now would drop the
      // message the user needs (story 31).
      if (event.key === "Escape" && !submitting) onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose, submitting]);

  /**
   * Ask about the launch method as the user writes it (#66).
   *
   * The question is about the *command*, and it is asked of the backend because
   * the answer is a fact about a file on this machine — which executable the
   * command resolves to, and whether that executable or the batch host around it
   * gets a console of its own. The name typed above is never part of the
   * question (spec #59 decision 9).
   *
   * The recommendation moves the choice until the user moves it themselves: an
   * entry starts on what the Hub could confirm, and stays wherever the user puts
   * it (story 39).
   */
  useEffect(() => {
    if (onRecommendDisplay === undefined) return;
    const settled = command.trim();
    if (settled === "") return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void onRecommendDisplay(settled, cwd.trim()).then((next) => {
        if (cancelled) return;
        setAdvice(next);
        setAdvisedFor(settled);
        if (next?.recommended !== undefined && !displayTouched) {
          // The recommendation moves the mode the same way a click would, so
          // it owes the same consequence: a policy the new mode cannot carry
          // has to go with it. Skipping that would leave `source: captured`
          // selected on a mode whose save the config layer refuses — the
          // "press does nothing" the choice exists to avoid.
          const recommended = next.recommended;
          setDisplay(recommended);
          setPolicy((current) => legalPolicyFor(recommended, current));
        }
      });
    }, RECOMMEND_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [command, cwd, displayTouched, onRecommendDisplay]);

  /** Change the display mode, and keep the logging policy legal for it. */
  const chooseDisplay = (mode: DisplayModeValue) => {
    setDisplayTouched(true);
    setDisplay(mode);
    setPolicy((current) => legalPolicyFor(mode, current));
    if (mode === "internal") setManageLifecycle(false);
  };

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (submitting || picking) return;

    const form: NewApplicationFormDto = {
      name: name.trim(),
      cwd: cwd.trim(),
      command: command.trim(),
    };
    const optional = [
      ["purpose", purpose],
      ["closeImpact", closeImpact],
    ] as const;
    for (const [key, value] of optional) {
      const trimmed = value.trim();
      if (trimmed !== "") form[key] = trimmed;
    }
    const fullUrl = applicationUrl(protocol, url);
    if (fullUrl !== undefined) form.url = fullUrl;
    if (port.trim() !== "") {
      const value = Number(port.trim());
      // Checked here rather than sent: the wire type is a 16-bit integer, so a
      // typo would come back as a deserialization failure instead of the
      // field's own message.
      if (!Number.isInteger(value) || value < 1 || value > 65535) {
        setError({ field: "port", message: "端口必须是 1-65535 之间的整数。" });
        return;
      }
      form.port = value;
    }
    const logging = loggingFor(policy, logPath);
    if (logging !== undefined) form.logging = logging;
    // Only what the user chose is written (#66): leaving the mode alone means
    // the file says nothing about it, exactly as every entry written before
    // these fields did.
    if (display === "window") {
      form.display = "window";
      if (manageLifecycle) form.lifecycle = "managed";
    }

    setError(null);
    setSubmitting(true);
    try {
      const result = await onSubmit(form);
      if (!result.ok) setError({ field: result.field, message: result.message });
    } finally {
      setSubmitting(false);
    }
  };

  const placedField = errorPlacement(error, policy);
  /** The advice, only while it is still about the command on screen. */
  const shownAdvice = advisedFor === command.trim() ? advice : null;

  /** The message for one input, when the refusal has been placed on it. */
  const errorFor = (field: string) => (placedField === field ? error?.message : null);

  return (
    <div className="dialog__backdrop" role="presentation">
      <form
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="add-application-title"
        onSubmit={submit}
      >
        <header className="dialog__head">
          <h2 className="dialog__title" id="add-application-title">
            添加应用
          </h2>
          <button
            type="button"
            className="dialog__close"
            aria-label="关闭"
            disabled={submitting}
            onClick={onClose}
          >
            <X size={14} />
          </button>
        </header>

        <p className="dialog__lead">
          保存一个启动配置；保存后会立即出现在会话列表里，下次打开 Hub 仍然可用。
        </p>

        <div className="dialog__body">
          <label className="dialog__field">
            <span className="dialog__label">
              名称 <span className="dialog__required">必填</span>
            </span>
            <input
              className="dialog__input"
              value={name}
              autoFocus
              onChange={(event) => {
                dirty.current.add("name");
                setName(event.target.value);
              }}
              placeholder="ComfyUI"
            />
            {errorFor("name") && <span className="dialog__field-error">{errorFor("name")}</span>}
          </label>

          <label className="dialog__field" htmlFor="application-cwd">
            <span className="dialog__label">
              工作目录 <span className="dialog__required">必填</span>
            </span>
            <div className="dialog__path">
              <input
                className="dialog__input dialog__input--mono"
                id="application-cwd"
                value={cwd}
                onChange={(event) => {
                  request.current += 1;
                  setPicking(false);
                  setScan(null);
                  setCwd(event.target.value);
                }}
                placeholder="D:\Tools\ComfyUI_windows_portable"
              />
              <button
                type="button"
                className="btn btn--secondary btn--sm"
                aria-label="选择应用目录"
                disabled={picking || !onPickPath}
                onClick={() => void choosePath("directory")}
              >
                <FolderOpen size={14} />
                选择目录
              </button>
            </div>
            <span className="dialog__hint">
              {picking ? "正在选择或扫描…" : "选择目录后查找启动文件；不会执行程序。"}
            </span>
            {errorFor("cwd") && <span className="dialog__field-error">{errorFor("cwd")}</span>}
          </label>

          <label className="dialog__field" htmlFor="application-command">
            <span className="dialog__label">
              启动命令 <span className="dialog__required">必填</span>
            </span>
            <div className="dialog__path">
              <input
                className="dialog__input dialog__input--mono"
                id="application-command"
                value={command}
                onChange={(event) => {
                  dirty.current.add("command");
                  setCommand(event.target.value);
                }}
                placeholder="python main.py"
              />
              <button
                type="button"
                className="btn btn--secondary btn--sm"
                aria-label="选择启动文件"
                disabled={picking || !onPickPath}
                onClick={() => void choosePath("program")}
              >
                <FileSearch size={14} />
                选择文件
              </button>
            </div>
            {errorFor("command") && (
              <span className="dialog__field-error">{errorFor("command")}</span>
            )}
          </label>

          {scan !== null && (
            <div className="dialog__discovery">
              {scan.candidates.length > 1 && (
                <label className="dialog__field">
                  <span className="dialog__label">找到多个启动文件，请选择</span>
                  <select
                    className="dialog__input"
                    aria-label="启动文件候选"
                    value=""
                    onChange={(event) => {
                      const candidate = scan.candidates.find(
                        (item) => item.path === event.target.value,
                      );
                      if (candidate) {
                        applyCandidate(candidate, true);
                        dirty.current.add("command");
                      }
                    }}
                  >
                    <option value="">选择要启动的程序或脚本</option>
                    {scan.candidates.map((candidate) => (
                      <option key={candidate.path} value={candidate.path}>
                        {candidate.path}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <span className="dialog__hint">
                {scan.candidates.length === 0
                  ? "未找到可确认的启动文件，可使用“选择文件”或手动填写。"
                  : scan.candidates.length === 1
                    ? "已找到启动文件，请确认预填内容后保存。"
                    : "候选仅按文件类型列出；请确认它是实际日常入口。"}
              </span>
              {scan.warnings.map((warning) => (
                <span key={warning} className="dialog__hint">
                  {warning}
                </span>
              ))}
            </div>
          )}
          {scanError !== null && (
            <p role="alert" className="dialog__error">
              {scanError}
            </p>
          )}

          <div className="dialog__row">
            <label className="dialog__field">
              <span className="dialog__label">端口</span>
              <input
                className="dialog__input dialog__input--mono"
                value={port}
                inputMode="numeric"
                onChange={(event) => {
                  dirty.current.add("port");
                  setPort(event.target.value);
                }}
                placeholder="8188"
              />
              {errorFor("port") && <span className="dialog__field-error">{errorFor("port")}</span>}
            </label>
            <label className="dialog__field" htmlFor="application-url">
              <span className="dialog__label">网页地址</span>
              <div className="dialog__address">
                <select
                  className="dialog__input"
                  aria-label="地址协议"
                  value={protocol}
                  onChange={(event) => setProtocol(event.target.value)}
                >
                  <option value="http://">http://</option>
                  <option value="https://">https://</option>
                </select>
                <input
                  className="dialog__input dialog__input--mono"
                  id="application-url"
                  aria-label="网页地址"
                  value={url}
                  onChange={(event) => {
                    dirty.current.add("url");
                    const value = event.target.value;
                    const match = value.match(/^(https?:\/\/)(.*)$/i);
                    if (match) {
                      setProtocol(match[1].toLowerCase());
                      setUrl(match[2]);
                    } else setUrl(value);
                  }}
                  placeholder="127.0.0.1:8188（可选）"
                />
              </div>
              {errorFor("url") && <span className="dialog__field-error">{errorFor("url")}</span>}
            </label>
          </div>

          <label className="dialog__field">
            <span className="dialog__label">用途</span>
            <input
              className="dialog__input"
              value={purpose}
              onChange={(event) => setPurpose(event.target.value)}
              placeholder="图像生成后端，队列里经常会有长时间任务。"
            />
            {errorFor("purpose") && (
              <span className="dialog__field-error">{errorFor("purpose")}</span>
            )}
          </label>

          <label className="dialog__field">
            <span className="dialog__label">关闭影响</span>
            <input
              className="dialog__input"
              value={closeImpact}
              onChange={(event) => setCloseImpact(event.target.value)}
              placeholder="停止会中断当前生成；队列中的任务会丢失。"
            />
            {errorFor("close_impact") && (
              <span className="dialog__field-error">{errorFor("close_impact")}</span>
            )}
          </label>

          <label className="dialog__field">
            <span className="dialog__label">日志策略</span>
            <select
              className="dialog__input"
              value={policy}
              onChange={(event) => setPolicy(event.target.value)}
            >
              {logPoliciesFor(display).map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
            <span className="dialog__hint">
              {logPoliciesFor(display).find((option) => option.value === policy)?.hint}
            </span>
            {errorFor("logging.source") && (
              <span className="dialog__field-error">{errorFor("logging.source")}</span>
            )}
          </label>

          {policy === "external" && (
            <label className="dialog__field" htmlFor="application-log">
              <span className="dialog__label">应用日志文件</span>
              <input
                className="dialog__input dialog__input--mono"
                id="application-log"
                value={logPath}
                onChange={(event) => {
                  dirty.current.add("log");
                  setLogPath(event.target.value);
                }}
                placeholder="D:\Tools\ComfyUI\logs\app.log"
              />
              <button
                type="button"
                className="btn btn--secondary btn--sm"
                disabled={picking || !onPickPath}
                onClick={() => void choosePath("log")}
              >
                选择日志文件
              </button>
              {errorFor("logging.path") && (
                <span className="dialog__field-error">{errorFor("logging.path")}</span>
              )}
            </label>
          )}

          {/* The display choice (#66), with what the Hub could confirm about
              this command beside it. The recommendation is advice, never a
              decision: the control is the user's, and the sentence says what was
              looked at so it can be checked rather than trusted. */}
          <div className="dialog__field">
            <span className="dialog__label">显示方式</span>
            <div className="dialog__choices" role="radiogroup" aria-label="显示方式">
              {DISPLAY_OPTIONS.map((option) => (
                <button
                  key={option.value}
                  type="button"
                  role="radio"
                  aria-checked={display === option.value}
                  className={`dialog__choice${display === option.value ? " dialog__choice--on" : ""}`}
                  onClick={() => chooseDisplay(option.value)}
                >
                  {option.label}
                </button>
              ))}
            </div>
            <span className="dialog__hint">{displayHint(display, shownAdvice)}</span>
          </div>

          {display === "window" && (
            <label className="dialog__field dialog__field--check">
              <input
                type="checkbox"
                checked={manageLifecycle}
                onChange={(event) => setManageLifecycle(event.target.checked)}
              />
              <span className="dialog__label">由 Hub 管理生命周期</span>
              <span className="dialog__hint">
                勾选后「停止全部」和退出 Hub 会一并停止它；不勾选时它由自己管理，退出 Hub
                不会关闭它。
              </span>
              {errorFor("lifecycle") && (
                <span className="dialog__field-error">{errorFor("lifecycle")}</span>
              )}
            </label>
          )}

          {error && placedField === null && (
            <p className="dialog__error" role="alert">
              {error.message}
            </p>
          )}
        </div>

        <footer className="dialog__foot">
          <button
            type="button"
            className="btn btn--secondary btn--sm"
            disabled={submitting}
            onClick={onClose}
          >
            取消
          </button>
          <button
            type="submit"
            className="btn btn--primary btn--sm"
            disabled={submitting || picking}
          >
            {submitting ? "正在保存…" : "保存应用"}
          </button>
        </footer>
      </form>
    </div>
  );
}
