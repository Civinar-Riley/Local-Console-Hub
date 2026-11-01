import { describe, expect, it } from "vitest";
import type { ListenerRowDto } from "../types/listen";
import { isPortOccupancyDto, type PortOccupancyDto } from "../types/port-occupancy";
import decisionSource from "./port-occupancy.ts?raw";
import {
  afterOccupancyCheck,
  afterPromptChoice,
  beforeOccupancyCheck,
  describeOccupants,
} from "./port-occupancy";

const sessions = [
  { id: "next", name: "即将启动" },
  { id: "holder", name: "已在运行" },
];

function row(overrides: Partial<ListenerRowDto> = {}): ListenerRowDto {
  return {
    protocol: "TCP",
    address: "0.0.0.0",
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

function occupied(rows: ListenerRowDto[]): PortOccupancyDto {
  return { prompt: true, port: 8188, failure: null, rows };
}

describe("beforeOccupancyCheck", () => {
  it("skips the prompt when the session has no configured port", () => {
    expect(beforeOccupancyCheck({ live: true, port: undefined, status: "stopped" })).toEqual({
      kind: "activate",
    });
    expect(beforeOccupancyCheck({ live: true, port: null, status: "exited" })).toEqual({
      kind: "activate",
    });
  });

  it("checks only a connected start that activate would actually start", () => {
    for (const status of ["stopped", "exited", "error"] as const) {
      expect(beforeOccupancyCheck({ live: true, port: 8188, status })).toEqual({ kind: "check" });
    }
  });

  it("does not ask when activate would only report the current run or refuse", () => {
    for (const status of ["starting", "running", "stopping"] as const) {
      expect(beforeOccupancyCheck({ live: true, port: 8188, status })).toEqual({
        kind: "activate",
      });
    }
  });

  it("keeps the preview path when the window is disconnected", () => {
    expect(beforeOccupancyCheck({ live: false, port: 8188, status: "stopped" })).toEqual({
      kind: "preview",
    });
  });
});

describe("afterOccupancyCheck", () => {
  it("starts when nothing is listening", () => {
    expect(afterOccupancyCheck({ prompt: false, port: 8188, failure: null, rows: [] })).toEqual({
      kind: "activate",
    });
  });

  it("prompts with each listener and does not collapse them", () => {
    const rows = [
      row({ protocol: "TCP", address: "0.0.0.0" }),
      row({ protocol: "UDP", address: "::", pid: 4243 }),
    ];
    expect(afterOccupancyCheck(occupied(rows))).toEqual({
      kind: "prompt",
      mode: "occupied",
      rows,
    });
  });

  it("treats a failed collection as unreadable, not an empty success and not a session", () => {
    const follow = afterOccupancyCheck({
      prompt: true,
      port: 8188,
      failure: "reading the TCP IPv4 owner table failed",
      rows: [row({ attribution: "session", sessionId: "holder" })],
    });
    expect(follow).toEqual({ kind: "prompt", mode: "unreadable" });
  });

  it("does not treat a prompt with no rows as a clear start", () => {
    expect(afterOccupancyCheck({ prompt: true, port: 8188, failure: null, rows: [] })).toEqual({
      kind: "prompt",
      mode: "unreadable",
    });
  });
});

describe("afterPromptChoice", () => {
  it("does not start on cancel and starts on continue", () => {
    expect(afterPromptChoice("cancel")).toBe("stay");
    expect(afterPromptChoice("continue")).toBe("activate");
  });
});

describe("describeOccupants", () => {
  it("names a managed session, and keeps external and unavailable distinct", () => {
    const lines = describeOccupants(
      [
        row({ attribution: "session", sessionId: "holder" }),
        row({ attribution: "external", sessionId: null, pid: 7 }),
        row({
          attribution: "unavailable",
          sessionId: null,
          pid: null,
          processName: null,
          programPath: null,
          address: "",
        }),
      ],
      sessions,
      "next",
    );

    expect(lines).toHaveLength(3);
    expect(lines[0].attribution).toBe("已在运行");
    expect(lines[0].attributionNeutral).toBe(false);
    expect(lines[1].attribution).toBe("外部");
    expect(lines[1].attributionNeutral).toBe(true);
    expect(lines[2].attribution).toBe("信息不可用");
    expect(lines[2].processName).toBe("信息不可用");
    expect(lines[2].pid).toBe("信息不可用");
    expect(lines[2].path).toBe("信息不可用");
    expect(lines[2].address).toBe("信息不可用");
    expect(lines[2].attributionNeutral).toBe(true);
    expect(lines[2].attribution).not.toBe("即将启动");
    expect(lines[2].attribution).not.toBe("已在运行");
  });

  it("does not name the session being started as its own occupant", () => {
    const [line] = describeOccupants(
      [row({ attribution: "session", sessionId: "next", processName: "即将启动" })],
      sessions,
      "next",
    );
    expect(line.attribution).toBe("信息不可用");
    expect(line.attributionNeutral).toBe(true);
    expect(line.processName).toBe("即将启动");
  });
});

describe("isPortOccupancyDto", () => {
  it("accepts a prompt and rejects a row that is not a listener", () => {
    expect(isPortOccupancyDto(occupied([row()]))).toBe(true);
    expect(isPortOccupancyDto({ prompt: false, port: null, failure: null, rows: [] })).toBe(true);
    expect(isPortOccupancyDto({ prompt: true, port: 1, failure: null, rows: [{ port: 1 }] })).toBe(
      false,
    );
  });
});

describe("occupancy decision source", () => {
  it("does not treat a health reading as ownership", () => {
    expect(decisionSource).not.toContain("portOpen");
    expect(decisionSource).not.toContain("health");
  });
});
