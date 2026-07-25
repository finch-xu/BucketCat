import * as RadixSelect from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";
import {
  groupOptions,
  type SelectGroupSpec,
  type SelectOption,
} from "./select-options";

export type { SelectGroupSpec, SelectOption };

/**
 * Themed single-select dropdown.
 *
 * Replaces the native `<select>` the connection form used to use: on macOS
 * that renders with the OS chrome, which reads as foreign next to this app's
 * light-blue theme. Radix supplies keyboard navigation, typeahead, focus
 * management, portalled positioning and ARIA listbox semantics; everything
 * visual here is project theme tokens, so light/dark follow automatically.
 *
 * Deliberately minimal: single-select only, no async loading, no multi-select.
 */
export function Select({
  value,
  onChange,
  options,
  groups,
  placeholder,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  groups?: SelectGroupSpec[];
  placeholder?: string;
  className?: string;
}) {
  const sections = groupOptions(options, groups);

  return (
    <RadixSelect.Root value={value} onValueChange={onChange}>
      <RadixSelect.Trigger
        className={cn(
          "flex h-9 w-full cursor-pointer items-center justify-between gap-2 rounded-[9px]",
          "border border-border bg-background px-3 text-[13px] text-fg2 outline-none",
          "focus:border-primary focus:ring-[3px] focus:ring-primary-soft",
          "data-[placeholder]:text-muted-foreground",
          className,
        )}
      >
        <RadixSelect.Value placeholder={placeholder} />
        <RadixSelect.Icon>
          <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
        </RadixSelect.Icon>
      </RadixSelect.Trigger>

      <RadixSelect.Portal>
        <RadixSelect.Content
          position="popper"
          sideOffset={4}
          className={cn(
            "z-50 max-h-[280px] min-w-[var(--radix-select-trigger-width)] overflow-hidden",
            "rounded-[9px] border border-border bg-panel shadow-[0_8px_24px_var(--shadow)]",
          )}
        >
          <RadixSelect.Viewport className="p-1">
            {sections.map((section, i) => {
              const items = section.options.map((o) => (
                <RadixSelect.Item
                  key={o.value}
                  value={o.value}
                  className={cn(
                    "relative flex cursor-pointer select-none items-center rounded-[6px]",
                    "py-1.5 pr-2 pl-7 text-[13px] text-foreground outline-none",
                    "data-[highlighted]:bg-hover",
                    "data-[state=checked]:bg-accent-tint data-[state=checked]:font-medium",
                  )}
                >
                  <RadixSelect.ItemIndicator className="absolute left-2 flex items-center">
                    <Check className="size-3.5 text-primary" />
                  </RadixSelect.ItemIndicator>
                  <RadixSelect.ItemText>{o.label}</RadixSelect.ItemText>
                </RadixSelect.Item>
              ));

              if (!section.group) return <div key={`loose-${i}`}>{items}</div>;
              return (
                <RadixSelect.Group key={section.group.key}>
                  <RadixSelect.Label className="px-2 py-1.5 text-[11px] font-medium text-muted-foreground">
                    {section.group.label}
                  </RadixSelect.Label>
                  {items}
                </RadixSelect.Group>
              );
            })}
          </RadixSelect.Viewport>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
