/**
 * Typed IPC layer over the Tauri connection commands.
 *
 * Mirrors `src-tauri/src/commands/connection.rs` and
 * `src-tauri/src/store/model.rs` exactly:
 * - Payload *struct* fields (ConnectionInput/ConnectionDto/Bucket) serialize
 *   with their Rust field names via serde -- snake_case, unchanged here.
 * - Command *argument* names get camelCased by Tauri's JS bridge, so
 *   `list_buckets(connection_id)` is invoked as `{ connectionId }` while
 *   `input`/`id` (no underscores) pass through unchanged.
 * - Every command rejects with `AppError`'s wire shape: `{ code, params }`
 *   (see `src-tauri/src/error.rs`). `toAppError` narrows whatever Tauri
 *   hands back into that shape, with an `internal` fallback for anything
 *   that doesn't conform.
 */
import { invoke } from "@tauri-apps/api/core";

/** Provider ids the UI knows about today (see `PROVIDERS` in
 * `src/lib/providers.ts`). The Rust side stores `provider` as a plain
 * `String` with no enum constraint, so this is a *soft* union: known ids
 * get autocomplete/exhaustiveness, but `(string & {})` keeps arbitrary
 * strings (e.g. from an older/future build) assignable without widening
 * the whole type to `string`. */
export type ProviderKind =
  | "s3"
  | "r2"
  | "minio"
  | "oss"
  | "cos"
  | "b2"
  | "generic"
  | (string & {});

/** Frontend -> backend payload for creating/updating a connection. Mirrors
 * `ConnectionInput` in `src-tauri/src/store/model.rs` field-for-field. */
export interface ConnectionInput {
  provider: ProviderKind;
  name: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  secret_access_key: string;
  default_bucket: string | null;
}

/** Backend -> frontend view of a saved connection (never the secret key).
 * Mirrors `ConnectionDto` in `src-tauri/src/store/model.rs`. */
export interface ConnectionDto {
  id: string;
  provider: ProviderKind;
  name: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  default_bucket: string | null;
}

/** A bucket as returned by `list_buckets`. Mirrors `Bucket` in
 * `src-tauri/src/provider/mod.rs`; `creation_date` is a pre-formatted
 * RFC 3339 string, not a Date. `region` is only ever set for Aliyun OSS
 * connections (whose native `ListBuckets` reports each bucket's own
 * region) -- `null`/absent for every other provider, where a connection's
 * buckets are all implicitly in the connection's configured region. */
export interface Bucket {
  name: string;
  creation_date: string | null;
  region?: string | null;
}

/** Wire shape of every command rejection -- `AppError`'s `Serialize` impl
 * in `src-tauri/src/error.rs`. `code` is a stable i18n key (e.g.
 * `"storage/bucket-not-found"`); `params` interpolates into its message. */
export interface AppError {
  code: string;
  params: Record<string, string>;
}

function isAppError(value: unknown): value is AppError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as { code?: unknown }).code === "string" &&
    typeof (value as { params?: unknown }).params === "object" &&
    (value as { params?: unknown }).params !== null
  );
}

/** Narrows an unknown Tauri rejection reason into an `AppError`, falling
 * back to a synthetic `internal` error for anything that doesn't conform
 * to the wire shape (e.g. a plain string, or an error thrown before the
 * command body ever ran). */
export function toAppError(reason: unknown): AppError {
  if (isAppError(reason)) return reason;
  const message =
    reason instanceof Error
      ? reason.message
      : typeof reason === "string"
        ? reason
        : String(reason);
  return { code: "internal", params: { message } };
}

async function invokeCommand<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (reason) {
    throw toAppError(reason);
  }
}

export function listConnections(): Promise<ConnectionDto[]> {
  return invokeCommand<ConnectionDto[]>("list_connections");
}

export function addConnection(input: ConnectionInput): Promise<ConnectionDto> {
  return invokeCommand<ConnectionDto>("add_connection", { input });
}

export function updateConnection(
  id: string,
  input: ConnectionInput,
): Promise<ConnectionDto> {
  return invokeCommand<ConnectionDto>("update_connection", { id, input });
}

export function deleteConnection(id: string): Promise<void> {
  return invokeCommand<void>("delete_connection", { id });
}

/** Tests connectivity for an as-yet-unsaved connection profile. Called
 * imperatively by the add-connection wizard -- not a query/mutation hook. */
export function testConnection(input: ConnectionInput): Promise<void> {
  return invokeCommand<void>("test_connection", { input });
}

export function listBuckets(connectionId: string): Promise<Bucket[]> {
  return invokeCommand<Bucket[]>("list_buckets", { connectionId });
}

/** One row of an object listing. Mirrors `ObjectEntry` in
 * `src-tauri/src/provider/mod.rs`: `is_prefix` marks a "folder" (common
 * prefix, `key` ends with "/", size/mtime/class are null); `size` is
 * numeric bytes and `last_modified` an RFC 3339 string — display
 * formatting happens in `src/lib/format.ts`. */
export interface ObjectEntry {
  key: string;
  name: string;
  size: number | null;
  last_modified: string | null;
  storage_class: string | null;
  is_prefix: boolean;
}

/** One page of `list_objects`. Feed `next_token` back as the next call's
 * `token` to continue; null means this was the last page. Mirrors
 * `ListPage` in `src-tauri/src/provider/mod.rs`. */
export interface ListPage {
  entries: ObjectEntry[];
  next_token: string | null;
}

/** A per-key failure inside a batch operation. `code` is an
 * `errors.*`-namespace i18n code (same space as `AppError.code`), not a
 * raw S3 code. Mirrors `FailedKey` in `src-tauri/src/provider/mod.rs`. */
export interface FailedKey {
  key: string;
  code: string;
}

/** Batch outcome:「成功 N / 失败 M」(design §7). Mirrors `BatchResult` in
 * `src-tauri/src/provider/mod.rs`. */
export interface BatchResult {
  succeeded: number;
  failed: FailedKey[];
}

/** Lists one page (backend-fixed page size) of objects + folders under
 * `prefix` — the current path plus any prefix-search text. */
export function listObjects(
  connectionId: string,
  bucket: string,
  prefix: string,
  token: string | null,
): Promise<ListPage> {
  return invokeCommand<ListPage>("list_objects", {
    connectionId,
    bucket,
    prefix,
    token,
  });
}

/** Batch-deletes object keys; resolves (not rejects) with per-key partial
 * failures in `BatchResult.failed`. */
export function deleteObjects(
  connectionId: string,
  bucket: string,
  keys: string[],
): Promise<BatchResult> {
  return invokeCommand<BatchResult>("delete_objects", { connectionId, bucket, keys });
}

/** Recursively deletes a folder: every object under `prefix` plus the
 * folder's own zero-byte marker object. Resolves (not rejects) with per-key
 * partial failures in `BatchResult.failed`, same design §7 contract as
 * `deleteObjects`. The backend rejects an empty `prefix` (a recursive delete
 * of the bucket root is never a UI gesture). */
export function deletePrefix(
  connectionId: string,
  bucket: string,
  prefix: string,
): Promise<BatchResult> {
  return invokeCommand<BatchResult>("delete_prefix", { connectionId, bucket, prefix });
}

/** Renames one object (backend implements copy + delete). */
export function renameObject(
  connectionId: string,
  bucket: string,
  fromKey: string,
  toKey: string,
): Promise<void> {
  return invokeCommand<void>("rename_object", { connectionId, bucket, fromKey, toKey });
}

/** Creates a folder: a zero-byte `prefix/` marker object. */
export function createFolder(
  connectionId: string,
  bucket: string,
  prefix: string,
): Promise<void> {
  return invokeCommand<void>("create_folder", { connectionId, bucket, prefix });
}

/** Generates a time-limited, unauthenticated GET URL for `key` (the Share
 * feature). `expiresSecs` is clamped server-side into S3's valid range
 * (1s..7d), so an out-of-range value here is never rejected -- it's just
 * silently bounded. The resolved URL carries a live signature: callers must
 * not log it. */
export function presignGet(
  connectionId: string,
  bucket: string,
  key: string,
  expiresSecs: number,
): Promise<string> {
  return invokeCommand<string>("presign_get", {
    connectionId,
    bucket,
    key,
    expiresSecs,
  });
}

/** An object's metadata (size, ETag, content type) for the details panel.
 * Mirrors `ObjectHead` in `src-tauri/src/provider/mod.rs`. */
export interface ObjectHead {
  size: number;
  etag: string | null;
  content_type: string | null;
}

/** Reads an object's metadata via a cheap `HeadObject` call. */
export function headObject(
  connectionId: string,
  bucket: string,
  key: string,
): Promise<ObjectHead> {
  return invokeCommand<ObjectHead>("head_object", { connectionId, bucket, key });
}

/** Where the bytes are going. Mirrors `Direction` in
 * `src-tauri/src/transfer/model.rs`. */
export type TransferDirection = "upload" | "download";

/** Task lifecycle. Mirrors `TransferStatus`. `completed` and `canceled` are
 * terminal -- the backend's state machine refuses to leave them. */
export type TransferStatus =
  | "queued"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "canceled";

/** One transfer task. Mirrors `TransferTaskDto` field-for-field. `seq` is a
 * creation counter used for newest-first ordering, not a timestamp. */
export interface TransferTask {
  id: string;
  seq: number;
  direction: TransferDirection;
  connection_id: string;
  bucket: string;
  key: string;
  local_path: string;
  file_name: string;
  total: number;
  transferred: number;
  status: TransferStatus;
  /** A bare `errors.*` code (e.g. `"network/timeout"`, no `errors.` prefix)
   * when `status === "failed"`, else null -- `useErrorText` prepends the
   * `errors.` namespace itself, so sending it pre-prefixed would double up
   * and never match a dictionary entry. */
  error_code: string | null;
}

/** One task's throttled progress. Mirrors `ProgressPayload`. `speed` is bytes
 * per second over the last 5 seconds; `eta_secs` is null when the transfer is
 * stalled or already done. */
export interface TransferProgress {
  task_id: string;
  transferred: number;
  total: number;
  speed: number;
  eta_secs: number | null;
}

/** Tauri event carrying a *batch* of `TransferProgress`, at most one per
 * 150ms. Batched so N concurrent tasks cost one IPC crossing, not N. */
export const TRANSFER_PROGRESS_EVENT = "transfer://progress";

/** Tauri event carrying a single `TransferTask` on every status change. Never
 * throttled -- a dropped `completed` would strand the panel at 99%. */
export const TRANSFER_STATE_EVENT = "transfer://state";

/** Queues one upload per local path, into `prefix` of `bucket`. Paths the
 * backend cannot use (gone, a directory) are skipped rather than failing the
 * whole batch, so the result may be shorter than `paths`. */
export function enqueueUploads(
  connectionId: string,
  bucket: string,
  prefix: string,
  paths: string[],
): Promise<TransferTask[]> {
  return invokeCommand<TransferTask[]>("enqueue_uploads", {
    connectionId,
    bucket,
    prefix,
    paths,
  });
}

/** Queues a single-file download into `localPath` (an absolute path the
 * frontend picked via the save dialog). The backend heads the object first,
 * so the returned task already carries the real `total`. */
export function enqueueDownload(
  connectionId: string,
  bucket: string,
  key: string,
  localPath: string,
): Promise<TransferTask> {
  return invokeCommand<TransferTask>("enqueue_download", {
    connectionId,
    bucket,
    key,
    localPath,
  });
}

/** Queues a recursive folder download: one task per real object under
 * `prefix`, reconstructing the subtree beneath `localDir` (an absolute path
 * the frontend picked via the directory dialog). Zero-byte folder markers are
 * skipped, and objects that resolve to no usable local path are dropped rather
 * than failing the batch, so the result may be shorter than the object count. */
export function enqueueFolderDownload(
  connectionId: string,
  bucket: string,
  prefix: string,
  localDir: string,
): Promise<TransferTask[]> {
  return invokeCommand<TransferTask[]>("enqueue_folder_download", {
    connectionId,
    bucket,
    prefix,
    localDir,
  });
}

export function listTransfers(): Promise<TransferTask[]> {
  return invokeCommand<TransferTask[]>("list_transfers");
}

export function pauseTransfer(taskId: string): Promise<void> {
  return invokeCommand<void>("pause_transfer", { taskId });
}

export function resumeTransfer(taskId: string): Promise<void> {
  return invokeCommand<void>("resume_transfer", { taskId });
}

export function cancelTransfer(taskId: string): Promise<void> {
  return invokeCommand<void>("cancel_transfer", { taskId });
}

export function retryTransfer(taskId: string): Promise<void> {
  return invokeCommand<void>("retry_transfer", { taskId });
}

/** Drops completed and canceled tasks. Paused/failed ones stay -- they are
 * still resumable/retriable. */
export function clearFinishedTransfers(): Promise<void> {
  return invokeCommand<void>("clear_finished_transfers");
}

/** Current value of the runtime resume/checkpoint flag (`ResumeFlag` in
 * `src-tauri/src/commands/settings.rs`), which gates whether the transfer
 * engine writes checkpoints at all. */
export function getResumeEnabled(): Promise<boolean> {
  return invokeCommand<boolean>("get_resume_enabled");
}

/** Sets the runtime resume/checkpoint flag and persists the choice to
 * `settings.json`, so it also takes effect on the next launch. Backed by
 * `apply_resume_setting` in `src-tauri/src/commands/settings.rs`. */
export function setResumeEnabled(enabled: boolean): Promise<void> {
  return invokeCommand<void>("set_resume_enabled", { enabled });
}

/** The full persisted app settings (M6c). Mirrors `Settings` in
 * `src-tauri/src/store/settings.rs` field-for-field. `max_tasks`/`max_parts`
 * are read by the transfer engine only at construction (app restart) --
 * there is no runtime hot-update. `share_expiry_secs` is frontend-consumed,
 * used to prefill the Share dialog's expiry field. */
export interface Settings {
  resume_enabled: boolean;
  max_tasks: number;
  max_parts: number;
  share_expiry_secs: number;
  close_to_tray: boolean;
}

/** Reads the whole persisted `Settings`, e.g. to initialize the Settings
 * modal. */
export function getSettings(): Promise<Settings> {
  return invokeCommand<Settings>("get_settings");
}

/** Persists a new max-concurrent-tasks limit. Backend clamps to `[1, 5]`
 * (`clamp_tasks` in `src-tauri/src/store/settings.rs`); takes effect on the
 * next app restart. */
export function setMaxTasks(n: number): Promise<void> {
  return invokeCommand<void>("set_max_tasks", { n });
}

/** Persists a new max-parts-per-task limit. Backend clamps to `[1, 8]`
 * (`clamp_parts`); takes effect on the next app restart. */
export function setMaxParts(n: number): Promise<void> {
  return invokeCommand<void>("set_max_parts", { n });
}

/** Persists a new default Share-link expiry, in seconds. Backend clamps to
 * `[1, 604800]` (`provider::clamp_expiry`), the same range `presignGet`'s
 * `expiresSecs` is bounded to. */
export function setShareExpiry(secs: number): Promise<void> {
  return invokeCommand<void>("set_share_expiry", { secs });
}

/** Current value of the runtime close-to-tray flag (`CloseToTrayFlag` in
 * `src-tauri/src/commands/settings.rs`), which the window's `CloseRequested`
 * handler consults to decide between hiding to the tray and quitting. */
export function getCloseToTray(): Promise<boolean> {
  return invokeCommand<boolean>("get_close_to_tray");
}

/** Sets the runtime close-to-tray flag and persists the choice, so it also
 * holds on the next launch. Backed by `apply_close_to_tray_setting`. */
export function setCloseToTray(enabled: boolean): Promise<void> {
  return invokeCommand<void>("set_close_to_tray", { enabled });
}

/** Whether the app is registered to launch at login.
 *
 * Read straight from the OS registration (a LaunchAgent plist on macOS) rather
 * than from `settings.json` -- deliberately not mirrored there, since the user
 * can remove the registration outside the app and a cached copy would then lie.
 * That is why this is not a field on `Settings`. */
export function getAutostart(): Promise<boolean> {
  return invokeCommand<boolean>("get_autostart");
}

/** Registers or unregisters launch-at-login. The registered command line
 * carries `--silent-start`, so a boot-time launch comes up in the tray instead
 * of opening a window. */
export function setAutostart(enabled: boolean): Promise<void> {
  return invokeCommand<void>("set_autostart", { enabled });
}

/** Replaces the tray menu's labels with localized ones. The tray is built in
 * Rust at startup with English fallbacks because the chosen locale lives only
 * in this webview's `localStorage`; see `src-tauri/src/tray.rs`. */
export function setTrayLabels(show: string, quit: string): Promise<void> {
  return invokeCommand<void>("set_tray_labels", { show, quit });
}

/** Result of `cleanCheckpointResidue`: how many orphan checkpoints were
 * removed, and how many bytes that (plus any staged `.bcpart`) freed. Mirrors
 * `CleanResult` in `src-tauri/src/commands/settings.rs`. */
export interface CleanResult {
  removed: number;
  freed_bytes: number;
}

/** Advanced-settings cleanup action (closes an M4c follow-up): removes every
 * checkpoint whose connection has been deleted -- and, for a download, its
 * staging `.bcpart` -- reusing the same orphan decision the startup restore
 * makes. Backed by `clean_checkpoint_residue` in
 * `src-tauri/src/commands/settings.rs`. */
export function cleanCheckpointResidue(): Promise<CleanResult> {
  return invokeCommand<CleanResult>("clean_checkpoint_residue");
}
