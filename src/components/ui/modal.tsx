import { useEffect, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export function Modal({
  onClose,
  className,
  children,
}: {
  onClose: () => void;
  className?: string;
  children: ReactNode;
}) {
  useEffect(() => {
    // An Escape already consumed by an overlay *inside* the modal (a Radix
    // Select's dropdown, say) must dismiss only that overlay, never the whole
    // modal -- closing it would throw away everything the user has typed.
    //
    // Radix's `DismissableLayer` registers its keydown on `document` with
    // `{ capture: true }` and, on Escape, calls `preventDefault()` + dismisses
    // WITHOUT calling `stopPropagation()`. Since the capture phase (window ->
    // document -> ... -> target) fully precedes the bubble phase (target ->
    // ... -> document -> window), that handler has always run by the time this
    // bubble-phase window listener fires, so `defaultPrevented` is reliably
    // `true` here for an Escape some layer already handled.
    //
    // Nothing else in this app calls `preventDefault()` on a keydown, so for a
    // modal with no open overlay Escape still closes it exactly as before.
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      onClick={onClose}
      className="absolute inset-0 z-40 flex items-center justify-center bg-[rgba(20,18,14,0.42)] backdrop-blur-[3px]"
    >
      <div
        onClick={(e) => e.stopPropagation()}
        className={cn(
          "max-h-[88%] overflow-y-auto rounded-2xl border border-border bg-background shadow-[0_34px_80px_-22px_rgba(0,0,0,0.55)] [animation:bc-pop_.2s_ease]",
          className,
        )}
      >
        {children}
      </div>
    </div>
  );
}
