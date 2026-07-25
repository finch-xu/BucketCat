import { useEffect, useId, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/utils";

export interface ComboboxOption {
  value: string;
  label: string;
  hint?: string;
  group?: string;
}

/** Same visual treatment as `connection-modal.tsx`'s `INPUT_CLASS`, kept as a
 * local constant so this component has no dependency on the modal. */
const INPUT_CLASS =
  "h-9 w-full rounded-[9px] border border-border bg-panel px-3 text-[13px] text-foreground outline-none focus:border-primary focus:ring-[3px] focus:ring-primary-soft";

type RenderItem =
  | { type: "header"; key: string; label: string }
  | { type: "option"; key: string; option: ComboboxOption; flatIndex: number };

/** A text input with a suggestions panel underneath -- NOT a select. Typing
 * anything calls `onChange` with exactly what was typed; the value is never
 * constrained to `options`. Built by hand because this project has no
 * dropdown/popover library and isn't taking on a new dependency for one. */
export function Combobox({
  value,
  onChange,
  options,
  placeholder,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  options: ComboboxOption[];
  placeholder?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const optionRefs = useRef<Map<number, HTMLLIElement>>(new Map());
  const listboxId = useId();

  // Case-insensitive substring match against value/label/hint, grouped by
  // `group` while preserving each group's first-appearance order (the
  // caller already hands us options pre-sorted by group).
  const { renderItems, flatOptions } = useMemo(() => {
    const q = value.trim().toLowerCase();
    const filtered = q
      ? options.filter((o) =>
          [o.value, o.label, o.hint].some((s) => s?.toLowerCase().includes(q)),
        )
      : options;

    const buckets = new Map<string, ComboboxOption[]>();
    const groupOrder: string[] = [];
    for (const o of filtered) {
      const key = o.group ?? "";
      let bucket = buckets.get(key);
      if (!bucket) {
        bucket = [];
        buckets.set(key, bucket);
        groupOrder.push(key);
      }
      bucket.push(o);
    }

    const items: RenderItem[] = [];
    const flat: ComboboxOption[] = [];
    for (const key of groupOrder) {
      const bucket = buckets.get(key);
      if (!bucket) continue;
      if (key) items.push({ type: "header", key: `h:${key}`, label: key });
      for (const o of bucket) {
        items.push({ type: "option", key: `o:${flat.length}:${o.value}`, option: o, flatIndex: flat.length });
        flat.push(o);
      }
    }
    return { renderItems: items, flatOptions: flat };
  }, [value, options]);

  // Keep the highlight index in range whenever the filtered list shrinks.
  useEffect(() => {
    if (highlight > flatOptions.length - 1) {
      setHighlight(Math.max(0, flatOptions.length - 1));
    }
  }, [flatOptions.length, highlight]);

  useEffect(() => {
    if (!open) return;
    function onDocMouseDown(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    optionRefs.current.get(highlight)?.scrollIntoView({ block: "nearest" });
  }, [open, highlight]);

  function selectOption(option: ComboboxOption) {
    onChange(option.value);
    setOpen(false);
    inputRef.current?.focus();
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    switch (e.key) {
      case "ArrowDown": {
        e.preventDefault();
        if (!open) {
          setOpen(true);
          return;
        }
        if (flatOptions.length === 0) return;
        setHighlight((h) => (h + 1) % flatOptions.length);
        return;
      }
      case "ArrowUp": {
        e.preventDefault();
        if (!open) {
          setOpen(true);
          return;
        }
        if (flatOptions.length === 0) return;
        setHighlight((h) => (h - 1 + flatOptions.length) % flatOptions.length);
        return;
      }
      case "Enter": {
        if (open && flatOptions[highlight]) {
          e.preventDefault();
          selectOption(flatOptions[highlight]);
        }
        return;
      }
      case "Escape": {
        // Close the panel only -- the input keeps whatever the user typed.
        // Stop propagation so the enclosing Modal's global Escape handler
        // doesn't also close the whole dialog on the same keypress.
        if (open) {
          e.stopPropagation();
          setOpen(false);
        }
        return;
      }
      case "Tab": {
        setOpen(false);
        return;
      }
      default:
        return;
    }
  }

  const activeOptionId =
    open && flatOptions[highlight] ? `${listboxId}-opt-${highlight}` : undefined;

  return (
    <div ref={containerRef} className={cn("relative", className)}>
      <input
        ref={inputRef}
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-controls={listboxId}
        aria-autocomplete="list"
        aria-activedescendant={activeOptionId}
        autoComplete="off"
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setHighlight(0);
        }}
        onFocus={() => setOpen(true)}
        onKeyDown={onKeyDown}
        placeholder={placeholder}
        className={INPUT_CLASS}
      />
      {open && flatOptions.length > 0 && (
        <ul
          id={listboxId}
          role="listbox"
          className="absolute z-50 mt-1 max-h-64 w-full overflow-y-auto rounded-[9px] border border-border bg-panel p-1 shadow-[0_10px_30px_-10px_rgba(0,0,0,0.35)]"
        >
          {renderItems.map((item) =>
            item.type === "header" ? (
              <li
                key={item.key}
                role="presentation"
                className="select-none px-2 pt-2 pb-1 text-[11px] font-semibold tracking-wide text-muted2 first:pt-1"
              >
                {item.label}
              </li>
            ) : (
              <li
                key={item.key}
                id={`${listboxId}-opt-${item.flatIndex}`}
                role="option"
                aria-selected={item.flatIndex === highlight}
                ref={(el) => {
                  if (el) optionRefs.current.set(item.flatIndex, el);
                  else optionRefs.current.delete(item.flatIndex);
                }}
                onMouseDown={(e) => {
                  // Prevent the input from blurring before the click lands.
                  e.preventDefault();
                  selectOption(item.option);
                }}
                onMouseEnter={() => setHighlight(item.flatIndex)}
                className={cn(
                  "flex cursor-pointer items-center justify-between gap-2 rounded-[7px] px-2 py-1.5 text-[13px]",
                  item.flatIndex === highlight
                    ? "bg-primary-soft text-foreground"
                    : "text-foreground hover:bg-hover",
                )}
              >
                <span className="truncate">{item.option.label}</span>
                {item.option.hint && (
                  <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
                    {item.option.hint}
                  </span>
                )}
              </li>
            ),
          )}
        </ul>
      )}
    </div>
  );
}
