import { cn } from "@/lib/utils";

export function Switch({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className={cn(
        "flex h-[23px] w-10 cursor-pointer rounded-xl border-none p-[2px] transition-colors duration-150",
        checked ? "bg-primary" : "bg-border",
      )}
    >
      <span
        className={cn(
          "size-[19px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform duration-150",
          checked && "translate-x-[17px]",
        )}
      />
    </button>
  );
}
