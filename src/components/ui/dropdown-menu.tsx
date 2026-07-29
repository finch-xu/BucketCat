import * as RadixDropdownMenu from "@radix-ui/react-dropdown-menu";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface DropdownItem {
  /** Stable React key. */
  key: string;
  label: ReactNode;
  onSelect: () => void;
}

/**
 * Themed action menu.
 *
 * Radix supplies arrow-key navigation, typeahead, focus return to the trigger,
 * outside-click dismissal, ARIA menu semantics and collision-aware positioning
 * (which is why the path bar's menu can sit at the bottom of the window and
 * still open upward); everything visual here is project theme tokens, so
 * light/dark follow automatically. Structure and classes mirror `select.tsx`
 * so the two overlays read as one family.
 *
 * DO NOT add a keydown handler that calls `stopPropagation()` on Escape.
 * `ui/modal.tsx` decides whether an Escape was already consumed by an overlay
 * *inside* it by reading `e.defaultPrevented` on a bubble-phase window
 * listener. That works because Radix's `DismissableLayer` calls
 * `preventDefault()` in the capture phase and lets the event keep propagating.
 * Stopping propagation here would break that contract and reintroduce the bug
 * fixed in d72ac8f (Escape closing the whole modal instead of just the
 * overlay).
 *
 * Deliberately minimal: flat list of actions, no submenus, no checkboxes.
 */
export function DropdownMenu({
  trigger,
  items,
  side = "bottom",
  align = "start",
  className,
}: {
  /** Rendered as the trigger via `asChild`, so pass a single focusable element. */
  trigger: ReactNode;
  items: DropdownItem[];
  side?: "top" | "bottom" | "left" | "right";
  align?: "start" | "center" | "end";
  className?: string;
}) {
  return (
    <RadixDropdownMenu.Root>
      <RadixDropdownMenu.Trigger asChild>{trigger}</RadixDropdownMenu.Trigger>

      <RadixDropdownMenu.Portal>
        <RadixDropdownMenu.Content
          side={side}
          align={align}
          sideOffset={4}
          collisionPadding={8}
          className={cn(
            "z-50 max-h-[280px] min-w-[180px] overflow-y-auto p-1",
            "rounded-[9px] border border-border bg-panel shadow-[0_8px_24px_var(--shadow)]",
            className,
          )}
        >
          {items.map((item) => (
            <RadixDropdownMenu.Item
              key={item.key}
              onSelect={item.onSelect}
              className={cn(
                "relative flex cursor-pointer select-none items-center rounded-[6px]",
                "px-2 py-1.5 text-[13px] text-foreground outline-none",
                "data-[highlighted]:bg-hover",
              )}
            >
              {item.label}
            </RadixDropdownMenu.Item>
          ))}
        </RadixDropdownMenu.Content>
      </RadixDropdownMenu.Portal>
    </RadixDropdownMenu.Root>
  );
}
