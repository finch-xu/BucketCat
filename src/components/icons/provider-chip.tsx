import { cn } from "@/lib/utils";
import type { ProviderMeta } from "@/lib/providers";

/**
 * 渲染一个 provider 的图标，一律着上它的品牌色。
 *
 * 着色收在这里而不是散在三个调用点：lucide 图标用 `currentColor` 描边，
 * 不着色就会渲染成 `text-*` 继承来的黑/白，丢掉品牌色 —— 而品牌色是这些
 * 中性线性图标之间**唯一**的区分手段（见 `providers.ts` 的图标规则：出于商标
 * 合规，这里不使用任何厂商的品牌 logo）。三处各判一次必然漂移。
 */
export function ProviderGlyph({
  meta,
  className,
}: {
  meta: ProviderMeta;
  className?: string;
}) {
  const Icon = meta.icon;
  return <Icon className={className} style={{ color: meta.color }} />;
}

/** 三个调用点用到的尺寸。数值沿用各处改造前的原值，避免顺手改动布局。 */
const CHIP_SIZES = {
  /** 连接向导第一步的九宫格卡片。 */
  lg: { chip: "size-[38px] rounded-[10px]", glyph: "size-5" },
  /** 连接向导第二步的头部。 */
  md: { chip: "size-[34px] rounded-[9px]", glyph: "size-5" },
  /** 侧栏连接行。 */
  sm: { chip: "size-[26px] rounded-[7px]", glyph: "size-[15px]" },
} as const;

/**
 * provider 图标 + 承托它的中性表面。
 *
 * 最早这里是「品牌色实底方块 + 白色 glyph」，后来为了承托彩色 logo 改成了中性
 * 表面。品牌 logo 现已因商标合规全部移除（见 `providers.ts`），但中性表面留了
 * 下来：品牌色现在落在 glyph 本身上（`ProviderGlyph`），底再上一层同色就糊了。
 * `bg-background` + `border-border` 让深浅两套主题都由现有 token 自动成立：
 *
 * - 浅色模式下 chip 是白的，衬在侧栏（`--sidebar`）、侧栏 hover（`--hover`）、
 *   向导卡片（`--panel`）上都能看出来；衬在已展开的连接行（`--raised`，同为
 *   白色）上则靠 `border-border` 勾出轮廓。
 * - 暗色模式下 chip 比 `--raised` / `--panel` 更深，同样有边框兜底。
 *
 * 刻意不加阴影：这三处都嵌在已有边框/阴影的卡片或行里，再叠一层会显脏。
 */
export function ProviderChip({
  meta,
  size,
  className,
}: {
  meta: ProviderMeta;
  size: keyof typeof CHIP_SIZES;
  className?: string;
}) {
  const s = CHIP_SIZES[size];
  return (
    <span
      className={cn(
        "flex shrink-0 items-center justify-center border border-border bg-background",
        s.chip,
        className,
      )}
    >
      <ProviderGlyph meta={meta} className={s.glyph} />
    </span>
  );
}
