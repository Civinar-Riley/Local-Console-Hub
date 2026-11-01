import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { watchMainWindowVisible } from "../../app/tauriWindowHost";
import {
  acceptListenResult,
  beginListenRefresh,
  checkCaption,
  completeListenRefresh,
  filterPorts,
  groupListedPorts,
  initialListenRefresh,
  nameListeners,
  noteWindowHidden,
  portListMessage,
  portSummary,
  selectedPort,
  shouldPollPorts,
  type ListenAttempt,
  type ListedPort,
  type SessionName,
  type SidebarView,
} from "../../state/ports";
import { createdAtFrom } from "../../state/confirm-end";
import { isListenerListDto, LIST_LISTENERS } from "../../types/listen";

/** Low-frequency while the page is up. Manual refresh does not wait for it. */
const PORT_POLL_MS = 8_000;

async function loadListeners(): Promise<ListenAttempt> {
  try {
    const value: unknown = await invoke(LIST_LISTENERS);
    if (!isListenerListDto(value)) {
      return { ok: false, failure: "端口列表的响应无法识别" };
    }
    if (value.failure !== null) {
      return { ok: false, failure: value.failure };
    }
    return { ok: true, checkedAtMs: value.checkedAtMs, rows: value.rows };
  } catch (error) {
    const message = error instanceof Error ? error.message : "端口列表没有返回";
    return { ok: false, failure: message };
  }
}

export interface PortListModel {
  query: string;
  setQuery: (query: string) => void;
  summary: string;
  groups: ReturnType<typeof groupListedPorts>;
  filtered: ListedPort[];
  selected: ListedPort | null;
  select: (key: string) => void;
  empty: string | null;
  caption: string;
  inProgress: boolean;
  refresh: () => void;
}

/**
 * The ports page's list: poll while it is showing and the window is visible,
 * and keep a hidden-period result from becoming the current check.
 */
export function usePortList(
  view: SidebarView,
  connected: boolean,
  sessions: readonly SessionName[],
  /** The host's clock, ticking a few times a minute. Rendered captions read
   * it rather than calling the clock during render. */
  nowMs: number,
): PortListModel {
  const [state, setState] = useState(initialListenRefresh);
  const [query, setQuery] = useState("");
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [windowVisible, setWindowVisible] = useState(true);
  const visibleRef = useRef(true);
  const viewRef = useRef(view);
  const ticketRef = useRef(0);

  useEffect(() => {
    viewRef.current = view;
  }, [view]);

  useEffect(() => {
    return watchMainWindowVisible((visible) => {
      if (visibleRef.current === visible) return;
      visibleRef.current = visible;
      if (!visible) {
        // Drop an attempt that started while the window was up. Its result
        // belongs to the hidden period and must not become the current check
        // when the window is shown again.
        ticketRef.current += 1;
        setState(noteWindowHidden);
      }
      setWindowVisible(visible);
    });
  }, []);

  const run = useCallback((manual: boolean) => {
    const visibleAtStart = visibleRef.current;
    if (!manual && !shouldPollPorts(viewRef.current, visibleAtStart, true)) return;
    const ticket = ++ticketRef.current;
    setState((current) => beginListenRefresh(current));
    void loadListeners().then((attempt) => {
      if (ticket !== ticketRef.current) return;
      const accept = acceptListenResult({
        manual,
        portsView: viewRef.current === "ports",
        visibleAtStart,
        visibleNow: visibleRef.current,
      });
      setState((current) => completeListenRefresh(current, attempt, accept));
    });
  }, []);

  useEffect(() => {
    if (!shouldPollPorts(view, windowVisible, connected)) return;
    run(false);
    const timer = window.setInterval(() => run(false), PORT_POLL_MS);
    return () => window.clearInterval(timer);
  }, [view, windowVisible, connected, run]);

  const named = useMemo(() => {
    const listed = nameListeners(state.rows, sessions);
    for (let index = 0; index < listed.length; index += 1) {
      const createdAt = createdAtFrom(state.rows[index] ?? {});
      if (createdAt !== null) Object.assign(listed[index], { createdAt });
    }
    return listed;
  }, [state.rows, sessions]);
  const filtered = useMemo(() => filterPorts(named, query), [named, query]);
  const selected = selectedPort(filtered, selectedKey);

  return {
    query,
    setQuery,
    summary: portSummary(named),
    groups: groupListedPorts(filtered),
    filtered,
    selected,
    select: setSelectedKey,
    empty: portListMessage(filtered, query, state),
    // The host's clock, not the check's own time: `justChecked` means "still
    // within a few seconds of the current check", and answering the check's
    // timestamp would make it true forever.
    caption: checkCaption(state, nowMs).text,
    inProgress: state.inProgress,
    refresh: () => {
      if (!connected) return;
      run(true);
    },
  };
}
