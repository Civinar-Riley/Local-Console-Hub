import {
  AppWindow,
  ExternalLink,
  FolderOpen,
  Pencil,
  Trash2,
  X,
  Play,
  RotateCw,
  Square,
} from "lucide-react";
import type { SessionConfigDto } from "../../types/config";
import type { SessionRuntimeDto } from "../../types/runtime";
import type { SessionAction } from "../../state/actions";
import {
  availableActions,
  headerCallout,
  metadataPairs,
  statusLabel,
  statusTone,
  typeLabel,
} from "../../state/derivations";
import "./SessionHeader.css";

export interface SessionHeaderProps {
  config: SessionConfigDto;
  runtime: SessionRuntimeDto;
  busy: boolean;
  ready: boolean;
  now: Date;
  /** What the control was asked for; the caller decides what it means. */
  onAction: (action: SessionAction) => void;
  closing: boolean;
}

/** Selected-session header: identity, state, close impact, metadata, actions. */
export default function SessionHeader({
  config,
  runtime,
  busy,
  ready,
  now,
  onAction,
  closing,
}: SessionHeaderProps) {
  const actions = availableActions(config, runtime);
  const callout = headerCallout(config, runtime);
  const metadata = metadataPairs(config, runtime, now);
  const tone = statusTone(runtime.status, busy);
  return (
    <header className="session-header">
      <div className="session-header__top">
        <div className="session-header__identity">
          <div className="session-header__title-row">
            <h1 className="session-header__name">{config.name}</h1>
            <span className="badge badge--outline">{typeLabel(config.sessionType)}</span>
            <span className={`badge badge--${tone}`}>
              <span className={`pip pip--${tone} badge__pip`} aria-hidden="true" />
              {statusLabel(runtime.status, busy, ready)}
            </span>
          </div>
          <p className="session-header__purpose">{config.purpose}</p>
        </div>
        <div className="session-header__actions">
          {/* The primary control of a run the Hub owns is 启动/停止. For a
              standalone application it is neither: stopping is not the Hub's
              to do (#66), so the control that remains is the one that works —
              opening the application, which is also what brings its own window
              forward when it is already running. */}
          {actions.start ? (
            <button
              type="button"
              className="btn btn--primary btn--sm"
              disabled={closing || actions.stopDisabled}
              onClick={() => onAction("start")}
            >
              <Play size={14} />
              启动
            </button>
          ) : actions.stop ? (
            <button
              type="button"
              className="btn btn--secondary btn--sm"
              disabled={closing || actions.stopDisabled}
              onClick={() => onAction("stop")}
            >
              <Square size={14} />
              停止
            </button>
          ) : (
            <button
              type="button"
              className="btn btn--primary btn--sm"
              disabled={closing || actions.stopDisabled}
              title="唤起应用的窗口"
              onClick={() => onAction("start")}
            >
              <AppWindow size={14} />
              打开
            </button>
          )}
          <button
            type="button"
            className="btn btn--secondary btn--sm"
            disabled={closing || !actions.restart}
            title={
              actions.associated
                ? "这个实例是在 Hub 之外启动的；Hub 没有启动它，也不会重启或结束它"
                : actions.managed
                  ? actions.restart
                    ? undefined
                    : "需等待上一次运行结束"
                  : "此应用由自己管理生命周期；在配置中启用 Hub 生命周期管理后可从这里重启"
            }
            onClick={() => onAction("restart")}
          >
            <RotateCw size={14} />
            重启
          </button>
          {actions.openUrl !== undefined && (
            <button
              type="button"
              className="btn btn--secondary btn--sm"
              title={actions.openUrl}
              onClick={() => onAction("open-url")}
            >
              <ExternalLink size={14} />
              打开网页
            </button>
          )}
          {actions.directory && (
            <button
              type="button"
              className="btn btn--secondary btn--sm"
              title={config.cwd}
              onClick={() => onAction("open-directory")}
            >
              <FolderOpen size={14} />
              目录
            </button>
          )}
          {actions.edit && (
            <button
              type="button"
              className="btn btn--ghost btn--sm"
              disabled={closing}
              title="编辑这个应用的启动配置"
              onClick={() => onAction("edit-application")}
            >
              <Pencil size={14} />
              编辑
            </button>
          )}
          <button
            type="button"
            className="btn btn--ghost btn--sm session-header__remove"
            disabled={closing || actions.stopDisabled || runtime.status === "starting"}
            title={
              config.temporary
                ? "结束这个临时终端并从列表移除"
                : "移除已保存的启动配置，不删除应用文件"
            }
            onClick={() => onAction(config.temporary ? "remove-session" : "remove-application")}
          >
            {config.temporary ? <X size={14} /> : <Trash2 size={14} />}
            {config.temporary ? (closing ? "正在关闭…" : "关闭") : "移除"}
          </button>
        </div>
      </div>

      {callout && (
        <div className={`callout callout--${callout.kind}`} role="note">
          <p className="callout__title">{callout.title}</p>
          <p className="callout__text">{callout.text}</p>
          {callout.note && <p className="callout__note">{callout.note}</p>}
        </div>
      )}

      <dl className="session-header__meta">
        {metadata.map((pair) => (
          <div key={pair.label} className="session-header__meta-pair">
            <dt>{pair.label}</dt>
            <dd title={pair.value}>{pair.value}</dd>
          </div>
        ))}
      </dl>
    </header>
  );
}
