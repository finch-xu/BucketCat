/**
 * Pure status-classification helpers for `TransferRow`
 * (`src/components/layout/transfer-bar.tsx`). Pulled out of the component so
 * the throttled/retrying branching -- easy to get subtly wrong, since a
 * throttled task is `status === "failed"` on the wire but must *not* render
 * like a real failure -- has unit coverage without needing to mount React.
 */

import type { TransferTask } from "./api";

/** The subset of `TransferTask` this module's logic reads. Narrower than the
 * full task so tests can build minimal fixtures. */
export type TaskStatusInfo = Pick<TransferTask, "status" | "error_code" | "notice">;

/** True when a failed task's failure was actually the backend backing off
 * under provider rate limiting (`error_code === "network/throttled"`, only
 * ever set when `status === "failed"` -- see `TransferTask.error_code` in
 * `src/lib/api.ts`). The panel renders this as an amber "paused, will
 * resume" state instead of a red failure: progress is preserved and a retry
 * is expected to succeed once the limiter backs off. */
export function isThrottled(task: TaskStatusInfo): boolean {
  return task.status === "failed" && task.error_code === "network/throttled";
}

/** True while a running task is being quietly retried by the backend
 * (`task.notice`, cleared the instant the task leaves `running` -- see
 * `TransferTask.notice`). Drives both the "retrying (n/max)" subtitle
 * prefix and its amber tone. */
export function hasRetryNotice(task: TaskStatusInfo): boolean {
  return task.status === "running" && task.notice !== null;
}

export type SubtitleTone = "warning" | "destructive" | "muted";

/** Tone for a row's subtitle line, and (via `colorFor`) its direction icon
 * and progress bar: amber for anything mid-retry -- throttled-and-paused or
 * quietly-retrying-while-running -- red for a real failure, muted for every
 * other status (including a plain running/queued/paused task). */
export function subtitleTone(task: TaskStatusInfo): SubtitleTone {
  if (isThrottled(task) || hasRetryNotice(task)) return "warning";
  if (task.status === "failed") return "destructive";
  return "muted";
}

/** Direction-icon / progress-bar color for a row. Keyed off the same tone
 * `subtitleTone` computes so the two never disagree, with the remaining
 * terminal statuses (`completed`, `canceled`) layered on top. */
export function colorFor(task: TaskStatusInfo): string {
  const tone = subtitleTone(task);
  if (tone === "warning") return "var(--warning)";
  if (tone === "destructive") return "var(--destructive)";
  if (task.status === "completed") return "var(--success)";
  if (task.status === "canceled") return "var(--muted2)";
  return "var(--primary)";
}

/** `title` text for the provenance badge: the full connection name and
 * bucket, untruncated. Falls back to the bare connection id (rather than a
 * translated placeholder) when the connection has been deleted, since this
 * is the hover-only long form meant for identifying *which* connection --
 * the visible truncated line uses a translated label instead (see
 * `TransferRowImpl`). */
export function provenanceTitle(
  connectionName: string | undefined,
  connectionId: string,
  bucket: string,
): string {
  return `${connectionName ?? connectionId} · ${bucket}`;
}
