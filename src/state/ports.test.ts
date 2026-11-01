import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import PortsWorkspace from "../components/ports/PortsWorkspace";
import workspaceSource from "../components/ports/PortsWorkspace.tsx?raw";
import hookSource from "../components/ports/usePortList.ts?raw";
import Sidebar from "../components/sidebar/Sidebar";
import type { ListenerRowDto } from "../types/listen";
import listenSource from "../types/listen.ts?raw";
import portsSource from "./ports.ts?raw";
import {
  acceptListenResult,
  beginListenRefresh,
  checkCaption,
  completeListenRefresh,
  EXTERNAL_LABEL,
  filterPorts,
  groupListedPorts,
  initialListenRefresh,
  nameListeners,
  noteWindowHidden,
  openableSessionId,
  ownerLabel,
  pathLabel,
  pidLabel,
  portListMessage,
  portSummary,
  shouldPollPorts,
  UDP_SOCKET_NOTE,
  UNAVAILABLE_LABEL,
  udpNote,
  type ListedPort,
} from "./ports";

const sessions = [
  { id: "comfy", name: "ComfyUI" },
  { id: "shell", name: "PowerShell" },
];

function raw(overrides: Partial<ListenerRowDto> & Pick<ListenerRowDto, "port">): ListenerRowDto {
  return {
    protocol: "TCP",
    address: "127.0.0.1",
    pid: 100,
    processName: "app.exe",
    programPath: null,
    attribution: "external",
    sessionId: null,
    createdAt: "133724480000000000",
    ...overrides,
  };
}

function listed(overrides: Partial<ListedPort> & Pick<ListedPort, "port">): ListedPort {
  const row = raw(overrides);
  const [named] = nameListeners(
    [
      {
        ...row,
        attribution: overrides.attribution ?? row.attribution,
        sessionId: overrides.sessionId ?? row.sessionId,
        processName: overrides.processName === undefined ? row.processName : overrides.processName,
      },
    ],
    sessions,
  );
  return { ...named, ...overrides, key: named.key };
}

describe("filterPorts", () => {
  const rows = nameListeners(
    [
      raw({ port: 9, processName: "alpha.exe", attribution: "session", sessionId: "comfy" }),
      raw({ port: 10, processName: "beta.exe", attribution: "external" }),
      raw({ port: 11, processName: "gamma.exe", attribution: "session", sessionId: "shell" }),
    ],
    sessions,
  );

  it("filters by port, process name, and session name", () => {
    expect(filterPorts(rows, "9").map((row) => row.port)).toEqual([9]);
    expect(filterPorts(rows, "ALPHA").map((row) => row.port)).toEqual([9]);
    expect(filterPorts(rows, "comfy").map((row) => row.port)).toEqual([9]);
    expect(filterPorts(rows, "PowerShell").map((row) => row.port)).toEqual([11]);
  });

  it("does not keep the previous rows when nothing matches", () => {
    const previous = filterPorts(rows, "alpha");
    expect(previous).toHaveLength(1);
    const empty = filterPorts(rows, "no-such-port");
    expect(empty).toEqual([]);
    expect(empty).not.toEqual(previous);
    expect(portListMessage(empty, "no-such-port", initialListenRefresh())).toBe("没有匹配的端口。");
  });
});

describe("checkCaption", () => {
  const previous = raw({ port: 9, processName: "kept.exe" });
  const now = 1_700_000_000_000;

  it("does not present a failure as a check that just finished", () => {
    const held = completeListenRefresh(
      initialListenRefresh(),
      { ok: true, checkedAtMs: now, rows: [previous] },
      true,
    );
    const failed = completeListenRefresh(
      beginListenRefresh(held),
      { ok: false, failure: "owner table failed" },
      true,
    );
    expect(failed.rows).toEqual([previous]);
    expect(failed.checkedAtMs).toBe(now);
    const caption = checkCaption(failed, now);
    expect(caption.justChecked).toBe(false);
    expect(caption.text).toContain("检查失败");
    expect(caption.text).toContain("上次成功");
    expect(caption.text).not.toContain("最近检查");
    const firstFailure = completeListenRefresh(
      beginListenRefresh(initialListenRefresh()),
      { ok: false, failure: "owner table failed" },
      true,
    );
    expect(portListMessage([], "", firstFailure)).toBe("这次检查没有得到列表。");
    expect(portListMessage(nameListeners(failed.rows, []), "", failed)).toBeNull();
  });

  it("does not label a hidden-window result as just checked", () => {
    const held = completeListenRefresh(
      initialListenRefresh(),
      { ok: true, checkedAtMs: now - 60_000, rows: [previous] },
      true,
    );
    const hidden = completeListenRefresh(
      beginListenRefresh(held),
      { ok: true, checkedAtMs: now, rows: [raw({ port: 99, processName: "hidden.exe" })] },
      false,
    );
    expect(hidden.rows).toEqual([previous]);
    expect(hidden.checkedAtMs).toBe(now - 60_000);
    expect(hidden.stale).toBe(true);
    const caption = checkCaption(hidden, now);
    expect(caption.justChecked).toBe(false);
    expect(caption.text).not.toContain("最近检查");
    expect(
      acceptListenResult({
        manual: false,
        portsView: true,
        visibleAtStart: true,
        visibleNow: false,
      }),
    ).toBe(false);
    expect(
      acceptListenResult({ manual: true, portsView: true, visibleAtStart: true, visibleNow: true }),
    ).toBe(true);
  });

  it("keeps the previous check when the window hides, and does not call it current", () => {
    const held = completeListenRefresh(
      initialListenRefresh(),
      { ok: true, checkedAtMs: now - 60_000, rows: [previous] },
      true,
    );
    const hidden = noteWindowHidden(beginListenRefresh(held));
    expect(hidden.rows).toEqual([previous]);
    expect(hidden.checkedAtMs).toBe(now - 60_000);
    expect(hidden.inProgress).toBe(false);
    expect(hidden.stale).toBe(true);
    const caption = checkCaption(hidden, now);
    expect(caption.justChecked).toBe(false);
    expect(caption.text).not.toContain("最近检查");
    expect(caption.text).toContain("上次检查");
    const again = noteWindowHidden(hidden);
    expect(again).toBe(hidden);
  });
});

describe("groupListedPorts", () => {
  it("keeps unavailable rows out of the external group", () => {
    const groups = groupListedPorts([
      listed({ port: 9, attribution: "session", sessionId: "comfy", processName: "alpha.exe" }),
      listed({ port: 10, attribution: "external", processName: "beta.exe" }),
      listed({ port: 11, attribution: "unavailable", pid: null, processName: null }),
    ]);
    expect(groups.map((group) => group.title)).toEqual(["受管", "外部", UNAVAILABLE_LABEL]);
    expect(groups.find((group) => group.title === "外部")?.rows.map((row) => row.port)).toEqual([
      10,
    ]);
    expect(groups.find((group) => group.id === "unavailable")?.rows.map((row) => row.port)).toEqual(
      [11],
    );
    expect(
      groupListedPorts([listed({ port: 10, attribution: "external" })]).map((g) => g.id),
    ).toEqual(["external"]);
  });
});

describe("openable sessions", () => {
  it("does not offer external or unavailable rows as sessions", () => {
    const managed = listed({
      port: 9,
      attribution: "session",
      sessionId: "comfy",
      processName: "alpha.exe",
    });
    const external = listed({ port: 10, attribution: "external", processName: "beta.exe" });
    const unavailable = listed({
      port: 11,
      attribution: "unavailable",
      pid: null,
      processName: null,
    });
    expect(openableSessionId(managed, sessions)).toBe("comfy");
    expect(openableSessionId(external, sessions)).toBeNull();
    expect(openableSessionId(unavailable, sessions)).toBeNull();
    expect(udpNote("UDP")).toBe(UDP_SOCKET_NOTE);
    expect(udpNote("TCP")).toBeNull();
  });
});

describe("polling", () => {
  it("polls only while the ports page is showing on a visible connected window", () => {
    expect(shouldPollPorts("ports", true, true)).toBe(true);
    expect(shouldPollPorts("ports", false, true)).toBe(false);
    expect(shouldPollPorts("sessions", true, true)).toBe(false);
    expect(shouldPollPorts("ports", true, false)).toBe(false);
  });
});

function buttonText(markup: string): string[] {
  return [...markup.matchAll(/<button\b[^>]*>([\s\S]*?)<\/button>/g)].map((match) =>
    match[1]
      .replace(/<[^>]+>/g, "")
      .replace(/\s+/g, " ")
      .trim(),
  );
}

function detailCard(markup: string): string {
  return markup.split('<article class="ports-card">')[1] ?? "";
}

function groupSection(markup: string, title: string): string {
  const marker = `>${title}</button></h2>`;
  const start = markup.indexOf(marker);
  const rest = markup.slice(start + marker.length);
  const next = rest.indexOf("sidebar__group-title");
  return next === -1 ? rest : rest.slice(0, next);
}

const END_CONTROL = /结束|停止|终止|kill|taskkill/i;

describe("listeners outside the Hub", () => {
  const other = listed({
    port: 11,
    attribution: "session",
    sessionId: "shell",
    processName: "pwsh.exe",
    pid: 55,
    programPath: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
  });
  const namedLikeASession = listed({
    port: 10,
    attribution: "external",
    processName: "PowerShell",
    pid: 77,
    programPath: "D:\\tools\\nginx.exe",
  });
  const external = listed({
    port: 10,
    attribution: "external",
    processName: "nginx.exe",
    pid: 77,
    programPath: "D:\\tools\\nginx.exe",
  });
  const unavailable = listed({
    port: 12,
    attribution: "unavailable",
    pid: null,
    processName: null,
    programPath: "   ",
  });
  const rows = [other, external, unavailable];

  it("labels an external row 外部 and does not offer it as the current session", () => {
    expect(ownerLabel(namedLikeASession)).toBe(EXTERNAL_LABEL);
    expect(namedLikeASession.sessionId).toBeNull();
    expect(namedLikeASession.sessionName).toBeNull();
    expect(openableSessionId(namedLikeASession, sessions)).toBeNull();
    expect(ownerLabel(external)).toBe(EXTERNAL_LABEL);
    expect(pidLabel(external.pid)).toBe("77");
    expect(pathLabel(external.programPath)).toBe("D:\\tools\\nginx.exe");

    const markup = renderToStaticMarkup(
      createElement(PortsWorkspace, {
        connected: true,
        rows,
        selected: external,
        caption: "最近检查 00:00:00",
        empty: null,
        inProgress: false,
        onSelect: vi.fn(),
        onRefresh: vi.fn(),
        onOpenSession: vi.fn(),
        openableSessionId: (row) => openableSessionId(row, sessions),
      }),
    );
    const detail = detailCard(markup);
    expect(detail).toContain("nginx.exe");
    expect(detail).toContain("PID 77");
    expect(detail).toContain("路径 D:\\tools\\nginx.exe");
    expect(detail).toContain(EXTERNAL_LABEL);
    expect(detail).toContain("不是当前会话");
    expect(detail).not.toContain("打开会话");
    expect(detail).not.toContain("PowerShell");
    expect(detail).not.toContain("ComfyUI");
    expect(buttonText(markup).join("\n")).not.toMatch(END_CONTROL);
    expect(buttonText(markup)).toEqual(["刷新"]);
    expect(markup).not.toContain("pip--run");
    expect(markup).not.toContain("pip--err");
    expect(markup).not.toContain("status-err");
    expect(markup).not.toContain("destructive");
  });

  it("shows another session's name on that session's row", () => {
    expect(other.sessionName).toBe("PowerShell");
    expect(ownerLabel(other)).toBe("PowerShell");
    expect(ownerLabel(other)).not.toBe(EXTERNAL_LABEL);
    expect(openableSessionId(other, sessions)).toBe("shell");

    const markup = renderToStaticMarkup(
      createElement(PortsWorkspace, {
        connected: true,
        rows,
        selected: other,
        caption: "最近检查 00:00:00",
        empty: null,
        inProgress: false,
        onSelect: vi.fn(),
        onRefresh: vi.fn(),
        onOpenSession: vi.fn(),
        openableSessionId: (row) => openableSessionId(row, sessions),
      }),
    );
    const detail = detailCard(markup);
    expect(detail).toContain("受管会话 PowerShell");
    expect(detail).toContain("pwsh.exe");
    expect(detail).toContain("PID 55");
    expect(detail).toContain("powershell.exe");
    expect(detail).not.toContain(`>${EXTERNAL_LABEL}<`);
    expect(buttonText(markup)).toEqual(["刷新", "打开会话"]);
    expect(buttonText(markup).join("\n")).not.toMatch(END_CONTROL);
  });

  it("does not fold an unread row into 外部", () => {
    expect(ownerLabel(unavailable)).toBe(UNAVAILABLE_LABEL);
    expect(ownerLabel(unavailable)).not.toBe(EXTERNAL_LABEL);
    expect(pidLabel(unavailable.pid)).toBe(UNAVAILABLE_LABEL);
    expect(pathLabel(unavailable.programPath)).toBe(UNAVAILABLE_LABEL);
    expect(pathLabel("   ")).toBe(UNAVAILABLE_LABEL);
    const groups = groupListedPorts(rows);
    expect(groups.map((group) => group.id)).toEqual(["managed", "external", "unavailable"]);
    expect(groups.find((group) => group.id === "external")).toMatchObject({
      title: EXTERNAL_LABEL,
      hint: "Hub 以外",
      rows: [external],
    });
    expect(groups.find((group) => group.id === "unavailable")?.rows).toEqual([unavailable]);
    expect(portSummary(rows)).toBe("3 监听 · 1 受管 · 1 外部");
    expect(portSummary([unavailable])).toBe("1 监听 · 0 受管 · 0 外部");

    const markup = renderToStaticMarkup(
      createElement(PortsWorkspace, {
        connected: true,
        rows,
        selected: unavailable,
        caption: "最近检查 00:00:00",
        empty: null,
        inProgress: false,
        onSelect: vi.fn(),
        onRefresh: vi.fn(),
        onOpenSession: vi.fn(),
        openableSessionId: (row) => openableSessionId(row, sessions),
      }),
    );
    const detail = detailCard(markup);
    expect(detail).toContain(UNAVAILABLE_LABEL);
    expect(detail).toContain("不把它当成外部");
    expect(detail).not.toContain("打开会话");
    expect(detail).not.toMatch(/ports-owner--neutral">外部/);
  });

  it("does not offer to end a process from the ports view", () => {
    const markup = renderToStaticMarkup(
      createElement(Sidebar, {
        groups: [],
        selectedId: "comfy",
        now: new Date(0),
        summary: "1 运行",
        query: "",
        onQueryChange: vi.fn(),
        onSelect: vi.fn(),
        onAdd: vi.fn(),
        onAddApplication: vi.fn(),
        view: "ports",
        portSummary: portSummary(rows),
        portGroups: groupListedPorts(rows),
        selectedPortKey: external.key,
        onSelectPort: vi.fn(),
      }),
    );
    expect(markup).toContain("Hub 以外");
    expect(markup).toContain(EXTERNAL_LABEL);
    expect(markup).toContain('aria-label="外部，已折叠"');
    expect(markup).toContain("PowerShell");
    expect(markup).toContain(UNAVAILABLE_LABEL);
    expect(markup).not.toContain("nginx.exe");
    expect(markup).not.toContain("PID 77");
    expect(markup).not.toContain("D:\\tools\\nginx.exe");
    expect(markup).not.toContain("ComfyUI");
    expect(markup).not.toContain("pip");
    expect(groupSection(markup, EXTERNAL_LABEL)).not.toContain(">12<");
    expect(groupSection(markup, UNAVAILABLE_LABEL)).not.toContain(">10<");
    expect(groupSection(markup, UNAVAILABLE_LABEL)).not.toContain(EXTERNAL_LABEL);
    expect(buttonText(markup).join("\n")).not.toMatch(END_CONTROL);
    expect(workspaceSource).not.toMatch(
      /结束进程|结束占用|强制结束|taskkill|force-stop|killProcess/,
    );
    expect(workspaceSource).not.toContain("pip--");
  });
});

describe("sample rows", () => {
  it("does not keep the prototype's sample listeners in the port sources", () => {
    const source = [portsSource, listenSource, workspaceSource, hookSource].join("\n");
    const banned = ["SillyTavern", "vertex-proxy", "svchost", "64728", "49670", "p8000"];
    for (const token of banned) {
      expect(source).not.toContain(token);
    }
    expect(workspaceSource).not.toContain("pip--run");
    expect(workspaceSource).not.toContain("pip--err");
  });
});
