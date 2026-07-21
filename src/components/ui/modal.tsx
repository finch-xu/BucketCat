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
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
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
