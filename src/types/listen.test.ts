import { describe, expect, it } from "vitest";
import { isListenerListDto } from "./listen";

describe("isListenerListDto", () => {
  it("accepts a success and a failure, and rejects a row that is not a listener", () => {
    expect(
      isListenerListDto({
        checkedAtMs: 10,
        inProgress: false,
        failure: null,
        rows: [
          {
            protocol: "TCP",
            address: "127.0.0.1",
            port: 1,
            pid: 2,
            processName: "app.exe",
            programPath: null,
            attribution: "external",
            sessionId: null,
            createdAt: "133724480000000000",
          },
        ],
      }),
    ).toBe(true);
    expect(
      isListenerListDto({
        checkedAtMs: null,
        inProgress: false,
        failure: "reading the TCP IPv4 owner table failed (os error 5)",
        rows: [],
      }),
    ).toBe(true);
    expect(
      isListenerListDto({ checkedAtMs: 10, inProgress: false, failure: null, rows: [{ port: 1 }] }),
    ).toBe(false);
  });
});
