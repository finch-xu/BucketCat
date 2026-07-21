import { cn } from "@/lib/utils";

export interface SegmentedOption<T extends string | number> {
  value: T;
  label: string;
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
          onClick={() => onChange(o.value)}
          className={cn(
            "cursor-pointer rounded-[7px] border-none px-[13px] py-[5px] text-[12.5px] font-medium transition-colors",
            o.value === value
              ? "bg-primary text-primary-foreground"
              : "bg-transparent text-fg2 hover:bg-hover",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
