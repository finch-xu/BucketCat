/**
 * Classifies a file for inline preview in the details panel. Pure and
 * side-effect free: extension lookup only, no IO. Reuses `extFromName`
 * from `format.ts` for the lowercased extension (same helper `fileMeta`
 * in `file-meta.ts` is keyed on).
 */

import { extFromName } from "./format";

export type PreviewKind = "image" | "video" | "audio" | "text" | "none";

/** Text files at or above this byte count render as "none" — too big to
 * fetch and preview inline. 1 MiB. */
export const TEXT_PREVIEW_MAX = 1_048_576;

const IMAGE_EXTS = new Set(["png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp"]);
const VIDEO_EXTS = new Set(["mp4", "webm", "mov"]);
const AUDIO_EXTS = new Set(["mp3", "wav", "ogg", "m4a"]);
const TEXT_EXTS = new Set([
  "txt",
  "md",
  "json",
  "xml",
  "csv",
  "js",
  "ts",
  "tsx",
  "jsx",
  "css",
  "html",
  "yml",
  "yaml",
  "log",
]);

/** Classifies `name` (and its `size` in bytes) into a preview kind. Text
 * files are only previewable under `TEXT_PREVIEW_MAX`; everything else
 * unrecognized (including extensionless names) is "none". */
export function previewKind(name: string, size: number): PreviewKind {
  const ext = extFromName(name);
  if (IMAGE_EXTS.has(ext)) return "image";
  if (VIDEO_EXTS.has(ext)) return "video";
  if (AUDIO_EXTS.has(ext)) return "audio";
  if (TEXT_EXTS.has(ext)) return size < TEXT_PREVIEW_MAX ? "text" : "none";
  return "none";
}
