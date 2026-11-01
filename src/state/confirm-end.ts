/**
 * Whether an occupying process may be ended (#110).
 *
 * Refresh, continue, and looking at the process do not end it. Cancel leaves
 * it running and does not change session state. Only an explicit confirm asks
 * the backend, and the backend checks the pid and creation time again.
 */

import {
  CONFIRM_END_OCCUPANT,
  isConfirmEndResult,
  type ConfirmEndResult,
  type EndTarget,
} from "../types/confirm-end";

const CREATED_AT = /^[0-9]+$/;

export type OccupantAction = "refresh" | "continue" | "view" | "cancel" | "confirm";

export function createdAtFrom(row: object): string | null {
  const value = (row as { createdAt?: unknown }).createdAt;
  if (typeof value !== "string" || !CREATED_AT.test(value)) return null;
  return value;
}

/**
 * The identity a confirm may name.
 *
 * Managed sessions are not offered this action: their own stop is unchanged.
 * A row without a pid or a creation time cannot be confirmed.
 */
export function endTargetOf(row: { pid: number | null; attribution: string }): EndTarget | null {
  if (row.attribution !== "external") return null;
  if (typeof row.pid !== "number" || !Number.isInteger(row.pid) || row.pid <= 0) return null;
  const createdAt = createdAtFrom(row);
  if (createdAt === null) return null;
  return { pid: row.pid, createdAt };
}

/** Refresh, continue, and the process view do not end anyone. Confirm does. */
export function occupantActionEnds(action: OccupantAction): boolean {
  return action === "confirm";
}

/**
 * Cancel does not ask for an end. The process stays, and session state is
 * not changed. Confirm asks for an end and still does not adopt or stop a
 * session; that part is the command's.
 */
export function afterEndConfirmation(choice: "cancel" | "confirm"): {
  requestEnd: boolean;
  processLeftRunning: boolean;
  sessionUnchanged: boolean;
} {
  if (choice === "cancel") {
    return { requestEnd: false, processLeftRunning: true, sessionUnchanged: true };
  }
  return { requestEnd: true, processLeftRunning: false, sessionUnchanged: true };
}

const NOTHING_ENDED = "没有结束。这次没有结束进程。";

/** Ask the backend to end `target` after the user has confirmed it. */
export async function requestConfirmedEnd(target: EndTarget): Promise<ConfirmEndResult> {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const value: unknown = await invoke(CONFIRM_END_OCCUPANT, {
      pid: target.pid,
      createdAt: target.createdAt,
    });
    if (!isConfirmEndResult(value)) return { ended: false, message: NOTHING_ENDED };
    if (!value.ended && !value.message.includes("没有结束")) {
      return { ended: false, message: `没有结束。${value.message}` };
    }
    return value;
  } catch (cause) {
    // The backend answers its own refusals in the payload above; a rejection
    // here is the transport itself. Saying only "nothing ended" would read as
    // a checked, deliberate keeping of the process rather than a request that
    // never got its answer.
    const reason = cause instanceof Error ? cause.message : String(cause);
    return { ended: false, message: `没有结束。这次请求没有送达后台：${reason}` };
  }
}
