/**
 * Pure display formatters for the real object model. The M2 mock carried
 * pre-formatted strings ("1.4 MB", "2026-07-18 09:22"); the real backend
 * sends numeric bytes and RFC 3339 timestamps (see `ObjectEntry` in
 * `src/lib/api.ts`), so formatting is now a frontend concern — kept pure
 * and unit-tested per design §8.
 */

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;

/** 1024-based human size: "312 B", "1.5 KB", "318 MB". One decimal,
 * trimmed when zero, dropped entirely for in-unit values >= 100. Unknown
 * (null/negative/NaN) renders as "—" — folders have no size. */
export function formatSize(bytes: number | null): string {
  if (bytes === null || !Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rendered =
    value >= 100 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, "");
  return `${rendered} ${UNITS[unit]}`;
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/** Local-time "YYYY-MM-DD HH:mm" for an RFC 3339 timestamp; "—" for
 * null/unparseable input. */
export function formatDate(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** Lowercased extension after the last "."; "" when there is none, when
 * the only dot leads the name (".gitignore"), or when it trails
 * ("weird."). Feeds `fileMeta` in `src/lib/file-meta.ts`. */
export function extFromName(name: string): string {
  const idx = name.lastIndexOf(".");
  if (idx <= 0 || idx === name.length - 1) return "";
  return name.slice(idx + 1).toLowerCase();
}
