import { cn } from "@/lib/utils";

export interface SegmentedOption<T extends string | number> {
  value: T;
  label: string;
  /** Renders this segment as display-only: no hover affordance, and clicking
   * it never calls `onChange`. Used for a state that can only be reached as
   * a side effect elsewhere (e.g. the "custom" transfer preset segment,
   * which the backend sets automatically once an advanced tuning field is
   * hand-edited -- it is never a choice the user picks directly). */
  disabled?: boolean;
}

export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  className,
}: {
  value: T;
  options: SegmentedOption<T>[];
  onChange: (value: T) => void;
  className?: string;
}) {
  return (
    <div className={cn("flex rounded-[9px] border border-border bg-panel p-[3px]", className)}>
      {options.map((o) => (
        <button
          key={String(o.value)}
          type="button"
          onClick={() => {
            if (!o.disabled) onChange(o.value);
          }}
          disabled={o.disabled}
          className={cn(
            "rounded-[7px] border-none px-[13px] py-[5px] text-[12.5px] font-medium transition-colors",
            o.disabled ? "cursor-default" : "cursor-pointer",
            o.value === value
              ? "bg-primary text-primary-foreground"
              : o.disabled
                ? "bg-transparent text-muted-foreground"
                : "bg-transparent text-fg2 hover:bg-hover",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
