import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { nameListeners } from "../../state/ports";
import type { ListenerRowDto } from "../../types/listen";
import PortsWorkspace from "./PortsWorkspace";

const sessions = [{ id: "shell", name: "PowerShell" }];

function raw(overrides: Partial<ListenerRowDto> & Pick<ListenerRowDto, "port">): ListenerRowDto {
  return {
    protocol: "TCP",
    address: "127.0.0.1",
    pid: 77,
    processName: "nginx.exe",
    programPath: "D:\\tools\\nginx.exe",
    attribution: "external",
    sessionId: null,
    createdAt: "133724480000000000",
    ...overrides,
  };
}

function buttonText(markup: string): string[] {
  return [...markup.matchAll(/<button\b[^>]*>([\s\S]*?)<\/button>/g)].map((match) =>
    match[1]
      .replace(/<[^>]+>/g, "")
      .replace(/\s+/g, " ")
      .trim(),
  );
}

function render(row: ReturnType<typeof nameListeners>[number]) {
  return renderToStaticMarkup(
    createElement(PortsWorkspace, {
      connected: true,
      rows: [row],
      selected: row,
      caption: "最近检查 00:00:00",
      empty: null,
      inProgress: false,
      onSelect: vi.fn(),
      onRefresh: vi.fn(),
      onOpenSession: vi.fn(),
      openableSessionId: () => (row.attribution === "session" ? row.sessionId : null),
    }),
  );
}

describe("ports confirm", () => {
  it("does not end from refresh or from viewing, and does not offer a session", () => {
    const external = nameListeners([raw({ port: 10 })], sessions)[0];
    Object.assign(external, { createdAt: "132" });
    const markup = render(external);

    expect(markup).toContain("刷新只重新读取");
    expect(markup).toContain("不是当前会话");
    expect(markup).not.toContain("确认结束");
    expect(buttonText(markup)).toEqual(["刷新", "结束此进程"]);

    const session = nameListeners(
      [
        raw({
          port: 11,
          attribution: "session",
          sessionId: "shell",
          processName: "pwsh.exe",
          pid: 55,
        }),
      ],
      sessions,
    )[0];
    Object.assign(session, { createdAt: "132" });
    expect(buttonText(render(session))).toEqual(["刷新", "打开会话"]);
  });
});
