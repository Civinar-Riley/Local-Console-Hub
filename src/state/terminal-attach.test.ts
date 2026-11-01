import { afterEach, describe, expect, it, vi } from "vitest";
import { openTerminalSession, RESIZE_SETTLE_MS, type TerminalBackend } from "./terminal-attach";
import type { TerminalAttachmentDto, TerminalOutputDto } from "../types/terminal";
import { encodeBase64 } from "../types/terminal";

/**
 * The attachment protocol (T07 #8), driven through a fake backend.
 *
 * These are the properties a view cannot check for itself at runtime: that
 * nothing published during the attachment is lost, that nothing the replay
 * already covered is shown twice, that typing cannot precede the attachment,
 * and that a resize reaches the backend once the view has settled.
 */

function attachment(overrides: Partial<TerminalAttachmentDto> = {}): TerminalAttachmentDto {
  return {
    sessionId: "term",
    ptyAttached: true,
    generation: 1,
    emitted: 4,
    chunks: [{ stream: "stdout", data: encodeBase64(new TextEncoder().encode("PS> ")) }],
    buffer: { bytes: 4, lines: 1, droppedBytes: 0 },
    ...overrides,
  };
}

function batch(overrides: Partial<TerminalOutputDto> = {}): TerminalOutputDto {
  return {
    sessionId: "term",
    generation: 1,
    start: 4,
    end: 9,
    data: encodeBase64(new TextEncoder().encode("hello")),
    ...overrides,
  };
}

/** A backend whose attachment the test resolves by hand. */
function fakeBackend(overrides: Partial<TerminalBackend> = {}) {
  let settleAttachment: (value: unknown) => void = () => {};
  let failAttachment: (reason: unknown) => void = () => {};
  const attached = new Promise<unknown>((resolve, reject) => {
    settleAttachment = resolve;
    failAttachment = reject;
  });

  let receive: ((payload: unknown) => void) | null = null;
  const written: string[] = [];
  const resized: Array<{ cols: number; rows: number }> = [];
  let unsubscribed = 0;

  const backend: TerminalBackend = {
    attach: () => attached,
    write: (_sessionId, data) => {
      written.push(new TextDecoder().decode(Uint8Array.from(atob(data), (c) => c.charCodeAt(0))));
      return Promise.resolve();
    },
    resize: (_sessionId, cols, rows) => {
      resized.push({ cols, rows });
      return Promise.resolve();
    },
    subscribe: (_sessionId, handler) => {
      receive = handler;
      return () => {
        unsubscribed += 1;
        receive = null;
      };
    },
    ...overrides,
  };

  return {
    backend,
    written,
    resized,
    publish: (payload: unknown) => receive?.(payload),
    resolveAttachment: (value: unknown = attachment()) => settleAttachment(value),
    rejectAttachment: (reason: unknown) => failAttachment(reason),
    get unsubscribed() {
      return unsubscribed;
    },
  };
}

/** The view's callbacks, recorded in the order they fire. */
function recorder() {
  const events: string[] = [];
  const rendered: string[] = [];
  return {
    events,
    rendered,
    handlers: {
      onAttach: (value: TerminalAttachmentDto) => {
        events.push("attach");
        rendered.push(...value.chunks.map((chunk) => atob(chunk.data)));
      },
      onData: (bytes: Uint8Array) => {
        events.push("data");
        rendered.push(new TextDecoder().decode(bytes));
      },
      onGap: () => events.push("gap"),
      onError: (message: string) => events.push(`error:${message}`),
    },
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("terminal attachment", () => {
  it("subscribes before it attaches, so nothing published meanwhile is lost", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    // Published while the attachment is still in flight.
    fake.publish(batch({ start: 4, end: 9 }));
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    expect(view.events).toEqual(["attach", "data"]);
    expect(view.rendered).toEqual(["PS> ", "hello"]);
    session.close();
  });

  it("drops a batch the replayed scrollback already covered", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    // The backend flushes what it has pending when a view attaches, so this is
    // the batch containing bytes the attachment just replayed.
    fake.publish(batch({ start: 0, end: 4 }));
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    expect(view.events).toEqual(["attach"]);
    expect(view.rendered).toEqual(["PS> "]);
    session.close();
  });

  it("does not forward input before the attachment exists", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    session.send("ls\r");
    expect(fake.written).toEqual([]);

    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    session.send("ls\r");
    expect(fake.written).toEqual(["ls\r"]);
    session.close();
  });

  it("sends the bytes a terminal sends, Ctrl+C included", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    session.send("\u0003");
    expect(fake.written).toEqual(["\u0003"]);
    session.close();
  });

  it("reports a gap instead of showing an incomplete stream", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    fake.publish(batch({ start: 40, end: 45 }));

    expect(view.events).toEqual(["attach", "gap", "data"]);
    session.close();
  });

  it("reports a failed attachment and stays detached", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    // The rejection a Tauri command throws: the structured SessionError the
    // backend serializes, whose message is what the view shows.
    fake.rejectAttachment({
      kind: "unknown_session",
      sessionId: "term",
      operation: "attach_terminal",
      message: "no session is registered as `term`",
    });
    await Promise.resolve();
    await Promise.resolve();

    expect(view.events).toEqual(["error:no session is registered as `term`"]);
    session.send("ls\r");
    expect(fake.written).toEqual([]);
    session.close();
  });

  it("ignores output belonging to another session", async () => {
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    fake.publish(batch({ sessionId: "other" }));
    expect(view.events).toEqual(["attach"]);
    session.close();
  });

  it("forwards a resize once the view has settled, not while it is dragging", async () => {
    vi.useFakeTimers();
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    session.resize(80, 24);
    session.resize(100, 30);
    session.resize(120, 32);
    expect(fake.resized).toEqual([]);

    vi.advanceTimersByTime(RESIZE_SETTLE_MS);
    expect(fake.resized).toEqual([{ cols: 120, rows: 32 }]);
    session.close();
  });

  it("does not repeat a size the backend already has", async () => {
    vi.useFakeTimers();
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);

    session.resize(120, 32);
    vi.advanceTimersByTime(RESIZE_SETTLE_MS);
    // The size is the view's only once the backend has answered it.
    await Promise.resolve();
    // A second observation of the same geometry — a layout pass, a scrollbar
    // appearing — is not a resize.
    session.resize(120, 32);
    vi.advanceTimersByTime(RESIZE_SETTLE_MS);
    await Promise.resolve();

    expect(fake.resized).toEqual([{ cols: 120, rows: 32 }]);
    session.close();
  });

  it("stops listening and stops sending once it is closed", async () => {
    vi.useFakeTimers();
    const fake = fakeBackend();
    const view = recorder();
    const session = openTerminalSession("term", fake.backend, view.handlers);
    fake.resolveAttachment();
    await Promise.resolve();
    await Promise.resolve();

    session.resize(90, 20);
    session.close();
    vi.advanceTimersByTime(RESIZE_SETTLE_MS);

    expect(fake.unsubscribed).toBe(1);
    expect(fake.resized).toEqual([]);
    expect(view.events).toEqual(["attach"]);
  });
});
