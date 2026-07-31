import { useEffect, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export function Modal({
  onClose,
  dismissOnOverlayClick = true,
  className,
  children,
}: {
  onClose: () => void;
  /** 点遮罩层是否关闭弹窗。默认 `true`，因为多数弹窗（删除确认、上传冲突、
   * 桶信息）里没有用户填过的内容，点空白关掉是顺手而非风险。
   *
   * 承载表单的弹窗应传 `false` —— 遮罩层用的是 `onClick`，而 DOM 的 click
   * 事件触发在 mousedown 与 mouseup 的**共同祖先**上，所以在输入框里拖选
   * 文字、松手时鼠标滑出了弹窗边界，click 就落在遮罩层上，弹窗关闭、已填
   * 内容全部丢失。用户不会觉得那是一次「点击」，只会觉得东西凭空没了。
   *
   * Escape 不受这个参数影响：主动敲键比鼠标滑出边界难误触得多，且是普遍
   * 约定。 */
  dismissOnOverlayClick?: boolean;
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
      onClick={dismissOnOverlayClick ? onClose : undefined}
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
