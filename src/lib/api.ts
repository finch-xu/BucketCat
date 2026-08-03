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
  | "rustfs"
  | "oss"
  | "rainyun"
  | "qiniu"
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
  /** Cloudflare API token value, for R2 connections only.
   *
   * When this is set and `secret_access_key` is blank, the **backend**
   * derives the secret as `sha256(token)` — Cloudflare's own relationship
   * between an R2 token and the S3 credentials it projects into. The
   * frontend deliberately never computes that hash (there is one
   * implementation, in `provider/r2.rs`, and the derived secret never exists
   * inside the webview).
   *
   * On edit, blank means "keep the stored token", exactly like
   * `secret_access_key`. Optional on the wire, so every non-R2 form keeps
   * sending the payload it always sent. */
  api_token?: string | null;
}

/** Backend -> frontend view of a saved connection (never the secret key,
 * never the API token). Mirrors `ConnectionDto` in
 * `src-tauri/src/store/model.rs`. */
export interface ConnectionDto {
  id: string;
  provider: ProviderKind;
  name: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  default_bucket: string | null;
  /** Whether a Cloudflare API token is stored for this connection. The token
   * itself never crosses this boundary; this boolean is all the edit form
   * needs to offer its "leave blank to keep" affordance. */
  has_api_token: boolean;
}

/** A bucket as returned by `list_buckets`. Mirrors `Bucket` in
 * `src-tauri/src/provider/mod.rs`; `creation_date` is a pre-formatted
 * RFC 3339 string, not a Date. `region` is only ever set for the providers
 * whose bucket list spans regions -- Aliyun OSS (its native `ListBuckets`
 * reports each bucket's own region) and Qiniu Kodo (resolved per bucket via
 * `GetBucketLocation`). `null`/absent for every other provider, where a
 * connection's buckets are all implicitly in the connection's configured
 * region. */
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

/** One account an R2 API token can see. Mirrors `R2Account` in
 * `src-tauri/src/provider/r2_admin.rs`. */
export interface R2Account {
  id: string;
  name: string;
}

/** What probing an R2 API token establishes. Mirrors `R2TokenProbe`. */
export interface R2TokenProbe {
  /** The token's own id, which **is** R2's S3 Access Key ID. */
  access_key_id: string;
  /** Accounts the token can enumerate.
   *
   * **Empty is a normal success, not a failure.** An R2 object-scoped token
   * verifies fine and reports its id, but Cloudflare answers `200 []` here
   * for it rather than 403 (verified live). The form must then ask the user
   * for the account id instead of rejecting the token. */
  accounts: R2Account[];
}

/** Probes an R2 API token the user has just pasted, before any connection
 * exists to save it against — returns the token's id (the S3 Access Key ID)
 * and whichever accounts it can see, so one paste fills in the access key,
 * the account id and the endpoint.
 *
 * Called imperatively by the connection form, like `testConnection`. */
export function r2ProbeToken(token: string): Promise<R2TokenProbe> {
  return invokeCommand<R2TokenProbe>("r2_probe_token", { token });
}

/** What probing a Backblaze B2 key pair establishes. Mirrors `B2KeyProbe` in
 * `src-tauri/src/provider/b2_admin.rs`. */
export interface B2KeyProbe {
  /** Region id, e.g. `us-west-004`. Straight from Backblaze's own `s3ApiUrl`,
   * so it may name a region this build's table doesn't list — that is how a
   * newly-launched B2 region works without an app update. */
  region: string;
  /** `https://s3.{region}.backblazeb2.com`. */
  endpoint: string;
  /** Buckets the key is restricted to.
   *
   * **Empty is the unrestricted case, not "no buckets".** Backblaze reports
   * `allowed.buckets: null` for a key with account-wide access (verified
   * live). A non-empty list only tells the user their key is scoped, which
   * explains a short bucket list later. */
  allowed_buckets: string[];
  /** Whether the key may call `ListBuckets`. A key without it still works for
   * object operations, so this drives a hint rather than an error. */
  can_list_buckets: boolean;
}

/** Probes a B2 `(keyID, applicationKey)` pair the user has just pasted, before
 * any connection exists to save it against — returns the account's
 * authoritative S3 region and endpoint.
 *
 * The form has already guessed both offline from the key id's cluster prefix
 * (see `b2RegionFromKeyId`, a convention Backblaze does not document); this is
 * what confirms or corrects that guess.
 *
 * Called imperatively by the connection form, like `testConnection`. */
export function b2ProbeKey(keyId: string, applicationKey: string): Promise<B2KeyProbe> {
  return invokeCommand<B2KeyProbe>("b2_probe_key", { keyId, applicationKey });
}

/** Bucket metadata from the Cloudflare API. Mirrors `R2BucketMeta`. */
export interface R2BucketMeta {
  /** R2's coarse location *hint* (`APAC`, `WNAM`, `WEUR`, …) — not a region.
   * R2's SigV4 region is always `auto`. */
  location: string | null;
  storage_class: string | null;
  jurisdiction: string | null;
}

/** Storage usage. Mirrors `R2Usage` — Cloudflare reports these as JSON
 * strings, but the backend has already parsed them to numbers. */
export interface R2Usage {
  object_count: number;
  payload_size: number;
  metadata_size: number;
  upload_count: number;
}

/** The `r2.dev` managed domain. `domain` is allocated even when `enabled` is
 * false — Cloudflare just doesn't serve it until public access is on. */
export interface R2ManagedDomain {
  enabled: boolean;
  domain: string;
}

export interface R2CustomDomain {
  domain: string;
  enabled: boolean;
  /** Cloudflare's SSL provisioning status. `enabled` with a non-`active`
   * status means configured but not yet serving. */
  ssl_status: string | null;
}

/** Everything the bucket-info panel shows. Mirrors `R2BucketInfo` in
 * `src-tauri/src/commands/r2.rs`.
 *
 * Each field is independently optional because the sources have genuinely
 * different availability: `location` comes from the **S3** plane and survives
 * every privilege tier, while everything below it needs a Cloudflare API
 * token and can be refused on its own. `api_error` is an `errors.*` i18n key
 * saying why the API-sourced fields are absent. */
export interface R2BucketInfo {
  bucket: string;
  location: string | null;
  has_api_token: boolean;
  meta: R2BucketMeta | null;
  usage: R2Usage | null;
  managed_domain: R2ManagedDomain | null;
  custom_domains: R2CustomDomain[] | null;
  api_error: string | null;
}

export function r2BucketInfo(
  connectionId: string,
  bucket: string,
): Promise<R2BucketInfo> {
  return invokeCommand<R2BucketInfo>("r2_bucket_info", { connectionId, bucket });
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
  /** Interpolation params for `error_code`'s i18n message (e.g. `{ bucket:
   * "photos" }`), mirroring the backend's `AppError::params()`. `null`
   * unless `error_code` is set. */
  error_params: Record<string, string> | null;
  /** A transient "this step is being retried" notice a runner emits while
   * `status === "running"`, distinct from `error_code`: it only ever shows up
   * mid-transfer and is cleared the moment the task leaves `running`, so it
   * never lingers on a finished, paused, failed or canceled row. `code` is an
   * `errors.*` key exactly like `error_code`; `attempt`/`max` let the panel
   * render e.g. "retrying (2/3)". */
  notice: { code: string; attempt: number; max: number } | null;
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

/** The full persisted app settings (M6c/M6d). Mirrors `Settings` in
 * `src-tauri/src/store/settings.rs` field-for-field. `max_tasks`/`max_parts`
 * and the six `transfer_preset`-linked tuning fields below all hot-apply to
 * the running transfer engine via `SharedLimits` -- no restart required.
 * `share_expiry_secs` is frontend-consumed, used to prefill the Share
 * dialog's expiry field. */
export interface Settings {
  resume_enabled: boolean;
  max_tasks: number;
  max_parts: number;
  share_expiry_secs: number;
  close_to_tray: boolean;
  update_source: string;
  auto_check_update: boolean;
  /** Which transfer tuning preset is active: `"conservative"`, `"balanced"`,
   * `"aggressive"`, or `"custom"` once any of the six fields below has been
   * hand-edited (never written by a `max_tasks`/`max_parts`-only change --
   * see `setMaxTasks`/`setMaxParts`). */
  transfer_preset: string;
  /** Files smaller than this upload as a single `PutObject`, in bytes. */
  upload_threshold: number;
  /** Lower bound on a computed upload part size, in bytes. */
  upload_part_floor: number;
  /** Target upload part count planning aims for. */
  upload_target_parts: number;
  /** Objects smaller than this download as a single Range GET, in bytes. */
  download_threshold: number;
  /** Lower bound on a computed download chunk size, in bytes. */
  download_chunk_floor: number;
  /** Target download chunk count planning aims for. */
  download_target_parts: number;
}

/** Reads the whole persisted `Settings`, e.g. to initialize the Settings
 * modal. */
export function getSettings(): Promise<Settings> {
  return invokeCommand<Settings>("get_settings");
}

/** Persists a new max-concurrent-tasks limit. Backend clamps to `[1, 5]`
 * (`clamp_tasks` in `src-tauri/src/store/settings.rs`) and hot-applies it to
 * the running transfer engine -- no restart needed. Does not change
 * `transfer_preset`. */
export function setMaxTasks(n: number): Promise<void> {
  return invokeCommand<void>("set_max_tasks", { n });
}

/** Persists a new max-parts-per-task limit. Backend clamps to `[1, 8]`
 * (`clamp_parts`) and hot-applies it the same way `setMaxTasks` does. Does
 * not change `transfer_preset`. */
export function setMaxParts(n: number): Promise<void> {
  return invokeCommand<void>("set_max_parts", { n });
}

/** Applies a built-in transfer tuning preset (spec §4.2): `"conservative"`,
 * `"balanced"`, or `"aggressive"`. The backend writes all six tuning fields
 * plus the preset's linked `max_tasks`/`max_parts` as one atomic group,
 * records the choice as `transfer_preset`, and hot-applies everything to the
 * running engine -- no restart. Rejects any other `name`. */
export function setTransferPreset(name: string): Promise<void> {
  return invokeCommand<void>("set_transfer_preset", { name });
}

/** Partial update to the six transfer tuning fields (spec §4.7's advanced
 * section) -- send only the one field the user just changed. Backend clamps
 * each provided value, flips `transfer_preset` to `"custom"`, and hot-applies
 * the result to the running engine. Mirrors `TransferTuningPatch` in
 * `src-tauri/src/commands/settings.rs` field-for-field (snake_case, like
 * `ConnectionInput`). */
export interface TransferTuningPatch {
  upload_threshold?: number;
  upload_part_floor?: number;
  upload_target_parts?: number;
  download_threshold?: number;
  download_chunk_floor?: number;
  download_target_parts?: number;
}

export function setTransferTuning(patch: TransferTuningPatch): Promise<void> {
  return invokeCommand<void>("set_transfer_tuning", { patch });
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

// --- In-app updates -------------------------------------------------------
//
// Every call below is a BucketCat command, not `@tauri-apps/plugin-updater`.
// The plugin is driven entirely from Rust (`src-tauri/src/commands/updater.rs`)
// so the update source can be switched without a restart -- and so this file
// stays the only place the frontend does IPC.

/** One selectable update source. Mirrors `UpdateSourceDto` in
 * `src-tauri/src/commands/updater.rs`. `id` is also the i18n lookup key
 * (`settings.updateSourceName.<id>`); `manifest_url` is display-only, shown so
 * users can see which host their app talks to. The list is a compile-time
 * constant on the Rust side -- users choose among these and cannot enter a URL
 * of their own. */
export interface UpdateSourceDto {
  id: string;
  manifest_url: string;
  /** Download page offered when this install cannot replace itself. Carried
   * per source so a future mirror can point somewhere reachable rather than
   * back at GitHub. */
  release_page_url: string;
}

/** What a successful update check found. Mirrors `UpdateInfo` in
 * `src-tauri/src/commands/updater.rs`. `body` is the manifest's release notes
 * and is often empty. `installable` is false only for a Linux `.deb`/`.rpm`
 * install, which cannot replace itself -- those users get a download link
 * instead of an install button. */
export interface UpdateInfo {
  version: string;
  current_version: string;
  body: string | null;
  installable: boolean;
}

/** Download-progress event payload, mirroring `UpdaterProgress` in
 * `src-tauri/src/commands/updater.rs`. Arrives as `started` once,
 * `progress` many times, then `finished` once. */
export type UpdaterProgressEvent =
  | { phase: "started"; content_length: number | null }
  | { phase: "progress"; chunk_length: number }
  | { phase: "finished" };

/** Name of the update download-progress event. */
export const UPDATE_PROGRESS_EVENT = "update://progress";

/** The built-in update sources, in display order. */
export function listUpdateSources(): Promise<UpdateSourceDto[]> {
  return invokeCommand<UpdateSourceDto[]>("list_update_sources");
}

/** Persists which built-in source to check. Rejects an id the backend does
 * not know rather than storing it. */
export function setUpdateSource(id: string): Promise<void> {
  return invokeCommand<void>("set_update_source", { id });
}

/** Persists whether to check for updates once on startup. */
export function setAutoCheckUpdate(enabled: boolean): Promise<void> {
  return invokeCommand<void>("set_auto_check_update", { enabled });
}

/** Checks the configured source. Resolves to `null` when already up to date;
 * rejects with `update/check-failed` when the manifest cannot be reached. */
export function checkForUpdate(): Promise<UpdateInfo | null> {
  return invokeCommand<UpdateInfo | null>("check_for_update");
}

/** Downloads and applies the update, emitting `UPDATE_PROGRESS_EVENT` as it
 * goes. Does not restart -- call `restartApp` once the user is ready. (On
 * Windows the NSIS installer terminates the process itself, so control may
 * never return here.) */
export function downloadInstallUpdate(): Promise<void> {
  return invokeCommand<void>("download_install_update");
}

/** Restarts into the freshly installed version. */
export function restartApp(): Promise<void> {
  return invokeCommand<void>("restart_app");
}
