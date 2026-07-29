/** Presentational building blocks shared by the four settings panes. No
 * state, no data fetching -- each pane owns its own. */

export function SectionTitle({ children, first }: { children: React.ReactNode; first?: boolean }) {
  return (
    <div
      className={`mb-3 text-[11px] font-semibold tracking-[0.6px] text-muted-foreground uppercase ${first ? "" : "mt-5"}`}
    >
      {children}
    </div>
  );
}

export function Row({ label, children }: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-2.5">
      <span className="text-[13.5px] text-fg2">{label}</span>
      {children}
    </div>
  );
}

/** Bounded +/- numeric stepper shared by the max-tasks and max-parts rows.
 * `onChange` only ever receives a value already inside `[min, max]` -- the
 * buttons clamp before calling it -- but callers still clamp again before
 * persisting, since this is also the value shown optimistically.
 * `disabled` is set by callers while their own persist is in flight so a
 * disabled button never fires `onClick` -- at most one persist per field can
 * ever be in flight, which rules out an out-of-order optimistic revert. */
export function Stepper({
  value,
  min,
  max,
  onChange,
  disabled = false,
}: {
  value: number;
  min: number;
  max: number;
  onChange: (n: number) => void;
  disabled?: boolean;
}) {
  return (
    <div className="flex items-center gap-0.5 overflow-hidden rounded-[9px] border border-border bg-panel">
      <button
        type="button"
        onClick={() => onChange(Math.max(min, value - 1))}
        disabled={disabled}
        className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
      >
        −
      </button>
      <span className="w-[34px] text-center text-[13px] font-semibold tabular-nums">{value}</span>
      <button
        type="button"
        onClick={() => onChange(Math.min(max, value + 1))}
        disabled={disabled}
        className="size-[30px] cursor-pointer text-base text-fg2 hover:bg-hover disabled:cursor-not-allowed disabled:opacity-60"
      >
        +
      </button>
    </div>
  );
}
