/** Display-layer folding and truncation for the path bar.
 *
 * Deliberately separate from `entries.ts`: that module owns the *semantics* of
 * an object key (prefix ↔ path ↔ basename, and the collision guards built on
 * them), and a bug there corrupts data. Nothing here can -- it only decides how
 * many characters of an already-correct path fit on screen. Keeping the two
 * apart keeps `entries.ts` focused and lets this file be read as pure
 * presentation. */

/** Whether a code point renders roughly twice as wide as a latin letter --
 * East Asian Wide/Fullwidth plus the emoji blocks.
 *
 * An approximation of UAX #11 East_Asian_Width, not a port of it: the real
 * table is thousands of ranges and would need a generated data file. These
 * ranges cover what actually shows up in object keys (CJK, kana, hangul,
 * fullwidth punctuation, emoji); anything unlisted falls back to width 1,
 * which errs toward *under*-estimating width and so toward truncating a little
 * less than necessary. CSS `truncate` on the rendered element is the backstop
 * for that case. */
function isWide(cp: number): boolean {
  return (
    (cp >= 0x1100 && cp <= 0x115f) || // Hangul Jamo
    (cp >= 0x2e80 && cp <= 0x303e) || // CJK Radicals .. CJK Symbols and Punctuation
    (cp >= 0x3041 && cp <= 0x33ff) || // Kana .. CJK Compatibility
    (cp >= 0x3400 && cp <= 0x4dbf) || // CJK Unified Ideographs Extension A
    (cp >= 0x4e00 && cp <= 0x9fff) || // CJK Unified Ideographs
    (cp >= 0xa000 && cp <= 0xa4cf) || // Yi
    (cp >= 0xac00 && cp <= 0xd7a3) || // Hangul Syllables
    (cp >= 0xf900 && cp <= 0xfaff) || // CJK Compatibility Ideographs
    (cp >= 0xfe30 && cp <= 0xfe6f) || // CJK Compatibility Forms
    (cp >= 0xff00 && cp <= 0xff60) || // Fullwidth Forms
    (cp >= 0xffe0 && cp <= 0xffe6) || // Fullwidth signs
    (cp >= 0x1f300 && cp <= 0x1faff) || // Emoji / pictographs
    (cp >= 0x20000 && cp <= 0x3fffd) // CJK Extension B and beyond
  );
}

/** Width of a single character (one code point, so possibly a surrogate pair). */
function charWidth(ch: string): number {
  const cp = ch.codePointAt(0);
  return cp !== undefined && isWide(cp) ? 2 : 1;
}

/** Approximate rendered width of `s` in "latin character" units: wide (CJK,
 * kana, hangul, fullwidth, emoji) counts 2, everything else counts 1.
 *
 * WHY NOT `s.length` (load-bearing): at this font size one CJK glyph is about
 * as wide as two latin ones, so `"备份-生产库-全量归档"` (11 chars) renders
 * WIDER than `"backup-production"` (17 chars). Budgeting by `.length` would
 * over-truncate latin names and let CJK names overflow. */
export function displayWidth(s: string): number {
  let total = 0;
  // Iterating the string yields code points, not UTF-16 code units, so an
  // astral-plane emoji counts once (as width 2) instead of twice.
  for (const ch of s) total += charWidth(ch);
  return total;
}

/** `s` shortened to at most `maxWidth` display units by dropping its middle
 * and joining the ends with an ellipsis. Returns `s` unchanged when it fits.
 *
 * WHY MIDDLE AND NOT TAIL: sibling folders in object storage routinely share a
 * long prefix and differ only in their suffix (`...-db-full` vs `...-db-incr`,
 * `day-1` vs `day-2`). Tail truncation renders those identically; keeping both
 * ends preserves the distinguishing part.
 *
 * WHY `Array.from` AND NOT `slice` (load-bearing): object keys are arbitrary
 * UTF-8. `String.prototype.slice` cuts on UTF-16 code units, so a cut landing
 * inside a surrogate pair leaves a lone surrogate that renders as U+FFFD
 * ("�"). Iterating code points makes an emoji indivisible.
 *
 * Fills the budget greedily from both ends (head first) rather than splitting
 * it in half: with wide characters, halving wastes up to one full slot per
 * side, so `middleTruncate("一二三四五六", 7)` yields `"一二…六"` (width 7)
 * instead of `"一…六"` (width 5). */
export function middleTruncate(s: string, maxWidth: number): string {
  // No room even for the ellipsis -- returning "…" here would itself overflow.
  if (maxWidth <= 0) return "";
  if (displayWidth(s) <= maxWidth) return s;

  const chars = Array.from(s);
  const budget = maxWidth - 1; // the ellipsis occupies one unit
  const head: string[] = [];
  const tail: string[] = [];
  let left = 0;
  let right = chars.length - 1;
  let used = 0;
  let toHead = true;

  while (left <= right) {
    const ch = chars[toHead ? left : right];
    const w = charWidth(ch);
    if (used + w > budget) break;
    used += w;
    if (toHead) {
      head.push(ch);
      left += 1;
    } else {
      tail.push(ch);
      right -= 1;
    }
    toHead = !toHead;
  }

  // `tail` was collected right-to-left.
  return `${head.join("")}…${tail.reverse().join("")}`;
}

/** Display-width budget for one crumb's label. The current folder gets the
 * larger one -- it is what the user actually reads to know where they are;
 * ancestors are mostly context. */
export const CURRENT_BUDGET = 36;
export const ANCESTOR_BUDGET = 24;

/** Pixel cost estimates for a crumb rendered at `text-[12.5px]`.
 *
 * `CHAR_PX` is per unit of `displayWidth`, and is deliberately a slight
 * OVER-estimate (a CJK glyph is 12.5px wide, i.e. 6.25 per unit; latin
 * averages ~6.5). Over-estimating makes the planner drop one crumb too early
 * rather than one too late, and dropping too late is what actually looks
 * broken: the row overflows and every short segment gets squeezed to "a…". */
const CHAR_PX = 6.8;
const CRUMB_PADDING_PX = 12; // px-1.5 on both sides
const SEPARATOR_PX = 14; // ChevronRight plus the gap around it
const BUCKET_ICON_PX = 19; // the Database icon plus its gap
const ELLIPSIS_PX = 26; // the "…" button

/** One navigable level. `index` is the position in the ORIGINAL crumb array
 * (0 = the bucket itself), which callers turn into `gotoCrumb(index - 1)` --
 * the store's convention where -1 means the bucket root.
 *
 * `label` is the display text (already middle-truncated); `full` is the
 * original. The planner truncates rather than leaving it to the caller so the
 * width it budgets with is exactly the width that gets rendered. */
export interface Crumb {
  kind: "crumb";
  label: string;
  full: string;
  index: number;
}

/** The single fold point, carrying every level it swallowed so the overflow
 * menu can still offer them as navigation targets. */
export interface CrumbEllipsis {
  kind: "ellipsis";
  hidden: Crumb[];
}

export type CrumbItem = Crumb | CrumbEllipsis;

/** Estimated rendered width of one crumb, separator excluded. */
function crumbPx(crumb: Crumb): number {
  const icon = crumb.index === 0 ? BUCKET_ICON_PX : 0;
  return displayWidth(crumb.label) * CHAR_PX + CRUMB_PADDING_PX + icon;
}

/** A render plan for `crumbs` inside `width` pixels: either every crumb, or
 * the bucket + one ellipsis + as many trailing crumbs as actually fit.
 *
 * WHY THIS BUDGETS PER SEGMENT (load-bearing): an earlier version divided the
 * width by a fixed per-segment average. Segment widths have a huge spread --
 * `07` and `2026-07-29-backup-of-production-db-full` are both one segment --
 * so an average let three long names blow the budget while the planner still
 * believed ten crumbs fit. The row overflowed and flex squeezed every short
 * segment down to "a…". Measuring each label instead keeps the total honest.
 *
 * Fills backwards from the current folder because that is the end that answers
 * "where am I": when something has to go, it should be an ancestor.
 *
 * No level is ever dropped -- folded ones move into the ellipsis's `hidden`,
 * so the path stays fully navigable however narrow the window gets. The
 * current folder is kept even when it alone overflows; `max-width` plus CSS
 * truncation handle that case visually. */
export function planCrumbs(crumbs: string[], width: number): CrumbItem[] {
  const last = crumbs.length - 1;
  const all: Crumb[] = crumbs.map((full, index) => ({
    kind: "crumb",
    full,
    label: middleTruncate(full, index === last ? CURRENT_BUDGET : ANCESTOR_BUDGET),
    index,
  }));
  if (all.length <= 1) return all;

  const total = all.reduce((sum, c, i) => sum + crumbPx(c) + (i > 0 ? SEPARATOR_PX : 0), 0);
  if (total <= width) return all;

  const head = all[0];
  let used = crumbPx(head) + SEPARATOR_PX + ELLIPSIS_PX;
  const tail: Crumb[] = [];
  for (let i = last; i >= 1; i -= 1) {
    const next = crumbPx(all[i]) + SEPARATOR_PX;
    // The first iteration is unconditional: the current folder is never the
    // crumb we drop, however narrow the bar gets.
    if (tail.length > 0 && used + next > width) break;
    used += next;
    tail.unshift(all[i]);
  }

  const hidden = all.slice(1, last + 1 - tail.length);
  // Everything fit after all (the ellipsis reservation was the only shortfall).
  if (hidden.length === 0) return [head, ...tail];
  return [head, { kind: "ellipsis", hidden }, ...tail];
}
