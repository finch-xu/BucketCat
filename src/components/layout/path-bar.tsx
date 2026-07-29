import { ChevronRight, Database } from "lucide-react";
import { Fragment, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { DropdownMenu } from "@/components/ui/dropdown-menu";
import { planCrumbs, type Crumb } from "@/lib/breadcrumb";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app-store";

function CrumbButton({
  crumb,
  isBucket,
  isCurrent,
  onClick,
}: {
  crumb: Crumb;
  isBucket: boolean;
  isCurrent: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={crumb.full}
      className={cn(
        "flex cursor-pointer items-center gap-[6px] rounded-[5px] px-1.5 py-[3px] text-[12.5px] hover:bg-hover",
        // The two ends are the answer to "where am I", so flex never steals
        // their width -- intermediate levels are compressed first, absorbing
        // whatever the planner's width estimate got wrong. `max-w` is what
        // keeps `shrink-0` from letting a long name overrun a narrow bar.
        isCurrent && "max-w-[45%] shrink-0 font-semibold text-foreground",
        isBucket && "max-w-[24%] shrink-0 font-medium text-muted-foreground",
        !isCurrent && !isBucket && "min-w-0 font-medium text-muted-foreground",
      )}
    >
      {isBucket && <Database className="size-[13px] shrink-0 text-muted-foreground" />}
      {/* The span is load-bearing: `text-overflow: ellipsis` applies to block
          containers, not to flex ones, so `truncate` on the button itself
          clips hard with no visible "…". Wrapping the text gives the ellipsis
          a block box to live in. */}
      <span className="truncate">{crumb.label}</span>
    </button>
  );
}

/**
 * The bottom path bar: where you are, and every level of it clickable.
 *
 * Lives below the file browser rather than in the toolbar because the toolbar's
 * right-hand controls (search + view toggle + three buttons) claim a rigid
 * ~463px, leaving the breadcrumb ~357px there against an unbounded path.
 * A row of its own is ~820px, so folding becomes a rare fallback instead of the
 * normal case.
 *
 * MOUNT POINT MATTERS: this must sit inside the content `<section>`, after the
 * FileBrowser/DetailsPanel row -- never inside the FileBrowser column. There
 * its width is (window - sidebar) and is therefore constant, independent of
 * both the details panel and of how many crumbs it renders. That is what makes
 * the ResizeObserver below safe: folding cannot change the width that decides
 * the folding, so there is no feedback loop and no hysteresis band is needed.
 */
export function PathBar() {
  const { t } = useTranslation();
  const { activeBucket, path, gotoCrumb } = useApp();
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    // Content width, i.e. excluding the bar's own horizontal padding -- the
    // same thing `contentRect` reports below, so the initial read and every
    // later one measure the same box.
    const style = getComputedStyle(el);
    const padding = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
    // Measured synchronously on mount: waiting for the observer's first
    // callback would paint one frame at width 0, i.e. maximally collapsed,
    // and then snap open.
    setWidth(el.clientWidth - padding);
    const observer = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const noBucket = activeBucket === "";
  // Same shape the toolbar's breadcrumb used: index 0 is the bucket itself, so
  // `gotoCrumb(index - 1)` maps it to the store's -1 ("bucket root").
  const crumbs = [activeBucket, ...path];
  const items = planCrumbs(crumbs, width);

  return (
    <div
      ref={ref}
      title={noBucket ? undefined : `${crumbs.join("/")}/`}
      className="flex h-7 shrink-0 items-center gap-px overflow-hidden border-t border-border bg-background px-4"
    >
      {noBucket ? (
        // Rendered even with no bucket so the bar keeps its 28px and the
        // layout does not jump when a bucket is picked.
        <span className="truncate text-[12.5px] text-muted-foreground">
          {t("main.breadcrumbPlaceholder")}
        </span>
      ) : (
        items.map((item, i) => (
          <Fragment key={item.kind === "ellipsis" ? "ellipsis" : `${item.index}-${item.label}`}>
            {i > 0 && <ChevronRight className="size-[13px] shrink-0 text-muted2" />}
            {item.kind === "ellipsis" ? (
              <DropdownMenu
                // The bar sits at the bottom of the window, so the menu opens
                // upward; Radix flips it back if it would collide anyway.
                side="top"
                align="start"
                trigger={
                  <button
                    type="button"
                    title={t("main.breadcrumbMore")}
                    aria-label={t("main.breadcrumbMore")}
                    className="flex shrink-0 cursor-pointer items-center rounded-[5px] px-1.5 py-[3px] text-[12.5px] font-medium text-muted-foreground hover:bg-hover"
                  >
                    …
                  </button>
                }
                items={item.hidden.map((hidden) => ({
                  key: `${hidden.index}-${hidden.label}`,
                  label: hidden.label,
                  onSelect: () => gotoCrumb(hidden.index - 1),
                }))}
              />
            ) : (
              <CrumbButton
                crumb={item}
                isBucket={item.index === 0}
                isCurrent={item.index === crumbs.length - 1}
                onClick={() => gotoCrumb(item.index - 1)}
              />
            )}
          </Fragment>
        ))
      )}
    </div>
  );
}
