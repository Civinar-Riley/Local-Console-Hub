import { useEffect, useState } from "react";
import { X } from "lucide-react";
import type { FormErrorDto, FormSaveOutcome, SaveTerminalFormDto } from "../../types/config";
import type { SessionConfigDto } from "../../types/config";
import "../dialog/dialog.css";
import "./SaveTerminalDialog.css";

export interface SaveTerminalDialogProps {
  /** The terminal being saved: the row's current name, shell and directory. */
  config: SessionConfigDto;
  /**
   * Save this terminal. The dialog stays open while this is in flight and
   * reports what came back — including a refusal, which is why it never
   * assumes the save succeeded (spec #59 decision 15: 只在持久保存确认后反馈
   * 成功).
   */
  onSubmit: (form: SaveTerminalFormDto) => Promise<FormSaveOutcome>;
  onClose: () => void;
}

/**
 * The fields this form has an input for.
 *
 * The backend's refusals name the *config* field, and two of the fields it can
 * name — `cwd` and `shell` — are shown here but not editable: they come from
 * the terminal, so there is no box to put a message beside. Those fall through
 * to the banner rather than disappearing (story 31: 保存失败有明确反馈).
 */
const RENDERED_FIELDS: ReadonlySet<string> = new Set(["name", "purpose", "close_impact"]);

/** Where a refusal belongs, or `null` for "in the banner". */
export function errorField(error: FormErrorDto | null): string | null {
  const field = error?.field;
  return field !== undefined && RENDERED_FIELDS.has(field) ? field : null;
}

/**
 * A shell path as a person reads it.
 *
 * The config's `shell` is a command line, so a path with a space in it —
 * `C:\Program Files\PowerShell\7\pwsh.exe`, the normal case for PowerShell 7 —
 * is stored quoted to survive the split into program and arguments
 * (`session::temporary::shell_command`). Showing the stored quotes in a
 * "this is what will run" line would read as part of the name.
 */
export function readableShell(shell: string | undefined): string {
  const trimmed = shell?.trim() ?? "";
  if (trimmed === "") return "—";
  return trimmed.length > 1 && trimmed.startsWith('"') && trimmed.endsWith('"')
    ? trimmed.slice(1, -1)
    : trimmed;
}

/**
 * The "保存启动配置" form (#65, spec #59 decisions 4 and 6).
 *
 * The other half of "新建 PowerShell": that entry opens a terminal nothing
 * remembers, and this keeps one. What it saves is the *launch method* — the
 * shell and the directory the terminal is already running in — plus a name the
 * user chooses, so the row can be found again after a restart.
 *
 * ## What it says out loud, and what it does not offer
 *
 * The launch method is shown, not asked for: it is what the running terminal
 * already resolved, and a form that could override it would be a second,
 * subtly different "添加应用". The two things a user might reasonably fear are
 * stated rather than implied — the running terminal is not restarted or
 * copied, and nothing they typed is saved or replayed (story 25). There is no
 * logging control either: a saved terminal keeps the `off`/`none` default an
 * interactive terminal has (`docs/LOGGING.md` §3), which is the same promise
 * the temporary terminal already made.
 */
export default function SaveTerminalDialog({ config, onSubmit, onClose }: SaveTerminalDialogProps) {
  const [name, setName] = useState(config.name);
  const [purpose, setPurpose] = useState(config.purpose ?? "");
  const [closeImpact, setCloseImpact] = useState(config.closeImpact ?? "");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<FormErrorDto | null>(null);

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

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (submitting) return;

    const form: SaveTerminalFormDto = { name: name.trim() };
    for (const [key, value] of [
      ["purpose", purpose],
      ["closeImpact", closeImpact],
    ] as const) {
      const trimmed = value.trim();
      if (trimmed !== "") form[key] = trimmed;
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

  const placedField = errorField(error);
  const errorFor = (field: string) => (placedField === field ? error?.message : null);

  return (
    <div className="dialog__backdrop" role="presentation">
      <form
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="save-terminal-title"
        onSubmit={submit}
      >
        <header className="dialog__head">
          <h2 className="dialog__title" id="save-terminal-title">
            保存启动配置
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
          把这个终端的启动方式保存成一条配置：保存后它立刻变成列表里的正式会话，下次打开 Hub
          仍然可用。终端不会重启，也不会被复制。
        </p>

        <div className="dialog__body">
          {/* Shown rather than asked for: this is what the terminal in front of
              the user already runs, and it is the whole content of the save. */}
          <dl className="save-terminal__method">
            <div className="save-terminal__method-row">
              <dt>工作目录</dt>
              <dd title={config.cwd}>{config.cwd ?? "—"}</dd>
            </div>
            <div className="save-terminal__method-row">
              <dt>shell</dt>
              <dd title={readableShell(config.shell)}>{readableShell(config.shell)}</dd>
            </div>
          </dl>

          <label className="dialog__field">
            <span className="dialog__label">
              名称 <span className="dialog__required">必填</span>
            </span>
            <input
              className="dialog__input"
              value={name}
              autoFocus
              onChange={(event) => setName(event.target.value)}
              placeholder="项目终端"
            />
            {errorFor("name") && <span className="dialog__field-error">{errorFor("name")}</span>}
          </label>

          <label className="dialog__field">
            <span className="dialog__label">用途</span>
            <input
              className="dialog__input"
              value={purpose}
              onChange={(event) => setPurpose(event.target.value)}
              placeholder="跑构建和脚本的终端。"
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
              placeholder="停止会结束这个终端启动的子进程。"
            />
            {errorFor("close_impact") && (
              <span className="dialog__field-error">{errorFor("close_impact")}</span>
            )}
          </label>

          {/* The promise the form is worth trusting for (story 25): what is
              being kept is the way this terminal starts, not what happened in
              it. */}
          <p className="save-terminal__promise">
            只保存 shell 和工作目录；不保存、不重放你输入过的命令，终端里的输出也不会被保存。
          </p>

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
          <button type="submit" className="btn btn--primary btn--sm" disabled={submitting}>
            {submitting ? "正在保存…" : "保存配置"}
          </button>
        </footer>
      </form>
    </div>
  );
}
