/**
 * The vocabulary of things a session control can ask for (T07 #8).
 *
 * A control reports *what it was asked to do*, not the words it rendered: the
 * label is presentation, and two buttons that happen to read the same would be
 * indistinguishable if the text were the protocol. The caller decides what an
 * action means — Session Core has an operation for the lifecycle ones, and the
 * rest are the UI's own business (or a later ticket's).
 */

/** Every action a session control can report. */
export type SessionAction =
  | "start"
  | "stop"
  | "restart"
  | "force-stop"
  | "open-url"
  | "open-directory"
  | "copy-path"
  | "new-session"
  | "save-config"
  | "edit-application"
  | "remove-session"
  | "remove-application";

/** The wording each action renders as, for the notices that name it. */
export const SESSION_ACTION_LABELS: Record<SessionAction, string> = {
  start: "启动",
  stop: "停止",
  restart: "重启",
  "force-stop": "强制结束进程树",
  "open-url": "打开网页",
  "open-directory": "打开目录",
  "copy-path": "复制路径",
  // The quick entry's own wording (spec #59 decision 7): one click opens an
  // interactive terminal, and the form-based "添加应用" entry is a different
  // control that does not exist yet.
  "new-session": "新建 PowerShell",
  // Saving a temporary terminal's launch method (#65): the other half of the
  // quick entry — this one keeps the shell and directory for next time.
  "save-config": "保存启动配置",
  // Editing a configured application's launch method: the same form that added
  // it, opened on what the file already holds.
  "edit-application": "编辑应用",
  "remove-session": "关闭临时终端",
  "remove-application": "从受管名单移除",
};
