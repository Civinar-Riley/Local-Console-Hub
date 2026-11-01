import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { describeOccupants } from "../../state/port-occupancy";
import type { ListenerRowDto } from "../../types/listen";
import PortOccupancyDialog from "./PortOccupancyDialog";

const sessions = [
  { id: "next", name: "即将启动" },
  { id: "holder", name: "已在运行" },
];

function row(overrides: Partial<ListenerRowDto> = {}): ListenerRowDto {
  return {
    protocol: "TCP",
    address: "127.0.0.1",
    port: 8188,
    pid: 4242,
    processName: "python.exe",
    programPath: "D:\\ComfyUI\\python.exe",
    attribution: "external",
    sessionId: null,
    createdAt: "133724480000000000",
    ...overrides,
  };
}

function render(mode: "occupied" | "unreadable", lines = describeOccupants([], sessions, "next")) {
  return renderToStaticMarkup(
    createElement(PortOccupancyDialog, {
      sessionName: "即将启动",
      port: 8188,
      mode,
      lines,
      onContinue: vi.fn(),
      onClose: vi.fn(),
    }),
  );
}

function buttonText(markup: string): string[] {
  return [...markup.matchAll(/<button\b[^>]*>(.*?)<\/button>/g)].map((match) => match[1]);
}

describe("PortOccupancyDialog", () => {
  it("shows each occupant and keeps external distinct from a managed session", () => {
    const markup = render(
      "occupied",
      describeOccupants(
        [
          row({ attribution: "session", sessionId: "holder", protocol: "TCP", address: "0.0.0.0" }),
          row({
            attribution: "external",
            protocol: "UDP",
            address: "::",
            pid: null,
            processName: null,
            programPath: null,
          }),
        ],
        sessions,
        "next",
      ),
    );

    expect(markup).toContain("即将启动");
    expect(markup).toContain("端口");
    expect(markup).toContain("8188");
    expect(markup).toContain("已在运行");
    expect(markup).toContain("外部");
    expect(markup).toContain("python.exe");
    expect(markup).toContain("4242");
    expect(markup).toContain("D:\\ComfyUI\\python.exe");
    expect(markup).toContain("TCP");
    expect(markup).toContain("0.0.0.0");
    expect(markup).toContain("UDP");
    expect(markup).toContain("::");
    expect(markup).toContain("信息不可用");
    expect(markup).toContain("port-occupancy__neutral");
    expect(markup).toContain("不会因此结束占用者");
    expect(markup).not.toContain("pip--run");
    expect(markup).not.toContain("dialog__error");
    expect(buttonText(markup)).toEqual(["取消", "仍然启动"]);
  });

  it("says the occupant could not be read, and does not name a session", () => {
    const markup = render("unreadable");

    expect(markup).toContain("信息不可用");
    expect(markup).toContain("不是一次空的检查");
    expect(markup).toContain("没有把它认成某个会话");
    expect(markup).not.toContain("已在运行");
    expect(markup).not.toContain("python.exe");
    expect(markup).not.toContain("pip--run");
    expect(buttonText(markup)).toEqual(["取消", "仍然启动"]);
  });

  it("offers an explicit confirm for an external identity, and not for a session", () => {
    const external = { ...row(), createdAt: "132" };
    const session = {
      ...row({ attribution: "session", sessionId: "holder" }),
      createdAt: "132",
    };
    const markup = render("occupied", describeOccupants([external, session], sessions, "next"));

    expect(markup).toContain("不会因此结束占用者");
    expect(markup).toContain("需要再确认一次");
    expect(markup).not.toContain("确认结束");
    expect(buttonText(markup)).toEqual(["结束此进程", "取消", "仍然启动"]);
  });
});
