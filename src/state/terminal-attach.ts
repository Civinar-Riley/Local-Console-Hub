/**
 * One view's attachment to a session's terminal stream (T07 #8).
 *
 * This is the protocol between a terminal view and the backend, written once
 * and without React so it can be tested directly: the React hook above it only
 * decides when a view exists, and the backend below it only moves bytes.
 *
 * ## The order that matters
 *
 * The view subscribes to output *before* it asks for the attachment. Output
 * published in between is therefore never lost: those batches are held, and
 * once the attachment arrives they go through the same rule the live ones do —
 * the ones the replayed scrollback already covered are dropped, the ones past
 * its offset are rendered (`src/state/terminal-stream.ts`). Subscribing
 * afterwards would leave a window in which the session talks and nobody who
 * cares is listening, and the user would find a hole in the middle of their
 * output with nothing to explain it.
 *
 * ## What the view can and cannot do
 *
 * Keystrokes and resizes are forwarded by name; the backend refuses input for
 * a session that is not running rather than swallowing it, and remembers a
 * size for a session that has not started yet. Nothing here decides either
 * policy — it reports what comes back.
 */

import { encodeBase64, isTerminalAttachmentDto, isTerminalOutputDto } from "../types/terminal";
import type { TerminalAttachmentDto, TerminalOutputDto } from "../types/terminal";
import { sessionErrorMessage } from "../types/runtime";
import { applyBatch, streamFromAttachment, type TerminalStreamState } from "./terminal-stream";

/**
 * How long a resize waits for the user to stop dragging.
 *
 * ConPTY reinitializes the console on every resize, and a keystroke that races
 * that reinitialization can be dropped by the console host — the quirk T02's
 * resize test documents. A drag produces dozens of sizes a second, so the view
 * settles first and tells the backend once (spec §14).
 */
export const RESIZE_SETTLE_MS = 150;

/** What the terminal data path needs from the backend. */
export interface TerminalBackend {
  /** The retained scrollback and the offset it reaches (`attach_terminal`). */
  attach(sessionId: string): Promise<unknown>;
  /** Input bytes, base64-encoded (`terminal_write`). */
  write(sessionId: string, data: string): Promise<void>;
  /** The view's size in cells (`terminal_resize`). */
  resize(sessionId: string, cols: number, rows: number): Promise<void>;
  /**
   * Subscribe to live output. Must deliver every batch published after this
   * call, and must be unsubscribed by the returned function.
   */
  subscribe(sessionId: string, receive: (payload: unknown) => void): () => void;
}

/** What the view does with what it is given. */
export interface TerminalSessionHandlers {
  /** Reset the surface and render this attachment's scrollback. */
  onAttach(attachment: TerminalAttachmentDto): void;
  /** Live bytes, in stream order, after `onAttach`. */
  onData(bytes: Uint8Array): void;
  /** Part of the stream was missed (see `terminal-stream.ts`). */
  onGap(): void;
  /** The view cannot be live, with the reason. */
  onError(message: string): void;
}

/** The handle a view keeps while it is attached. */
export interface TerminalSession {
  /** Send keystrokes (a paste included). A no-op until the view is attached. */
  send(text: string): void;
  /** Report the view's size, once it has stopped changing. */
  resize(cols: number, rows: number): void;
  /** Detach: stop listening, and stop forwarding anything. */
  close(): void;
}

/**
 * Attach a view to `sessionId` and start feeding it.
 *
 * Returns synchronously — the attachment itself is asynchronous, and the view
 * is alive (and receiving held batches) before it resolves. Until then `send`
 * is a no-op: there is no stream to type into yet, and forwarding keystrokes to
 * a session the view has not attached to would be guessing at what the user
 * meant.
 */
export function openTerminalSession(
  sessionId: string,
  backend: TerminalBackend,
  handlers: TerminalSessionHandlers,
): TerminalSession {
  let position: TerminalStreamState | null = null;
  let closed = false;
  /** Batches that arrived before the attachment resolved. */
  const held: TerminalOutputDto[] = [];

  const deliver = (payload: unknown) => {
    if (closed || !isTerminalOutputDto(payload) || payload.sessionId !== sessionId) {
      return;
    }
    if (position === null) {
      held.push(payload);
      return;
    }
    try {
      const outcome = applyBatch(position, payload);
      position = outcome.state;
      if (outcome.state.gapped) {
        handlers.onGap();
      }
      if (outcome.bytes !== null) {
        handlers.onData(outcome.bytes);
      }
    } catch (cause) {
      // A payload that will not decode is a broken contract rather than a
      // stream to keep spending on: report it and leave the view where it is.
      handlers.onError(sessionErrorMessage(cause));
    }
  };

  // Subscription first — see the module note.
  const unsubscribe = backend.subscribe(sessionId, deliver);

  void backend
    .attach(sessionId)
    .then((payload) => {
      if (closed) {
        return;
      }
      if (!isTerminalAttachmentDto(payload)) {
        throw new Error(`attach_terminal 返回了无法识别的载荷：${sessionId}`);
      }
      position = streamFromAttachment(payload);
      handlers.onAttach(payload);
      for (const batch of held.splice(0)) {
        deliver(batch);
      }
    })
    .catch((cause: unknown) => {
      if (!closed) {
        handlers.onError(sessionErrorMessage(cause));
      }
    });

  let settling: ReturnType<typeof setTimeout> | null = null;
  let reported: { cols: number; rows: number } | null = null;

  return {
    send(text: string) {
      if (closed || position === null) {
        return;
      }
      const data = encodeBase64(new TextEncoder().encode(text));
      backend
        .write(sessionId, data)
        .catch((cause: unknown) => handlers.onError(sessionErrorMessage(cause)));
    },

    resize(cols: number, rows: number) {
      if (closed || cols < 1 || rows < 1) {
        return;
      }
      if (reported !== null && reported.cols === cols && reported.rows === rows) {
        return;
      }
      if (settling !== null) {
        clearTimeout(settling);
      }
      settling = setTimeout(() => {
        settling = null;
        // Remembered only once the backend has taken the size: one it refused
        // was never applied, and recording it before the answer would stop the
        // next observation of the same size from retrying it.
        backend
          .resize(sessionId, cols, rows)
          .then(() => {
            reported = { cols, rows };
          })
          .catch((cause: unknown) => handlers.onError(sessionErrorMessage(cause)));
      }, RESIZE_SETTLE_MS);
    },

    close() {
      closed = true;
      if (settling !== null) {
        clearTimeout(settling);
        settling = null;
      }
      unsubscribe();
    },
  };
}
