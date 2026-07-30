import { cn } from "@/lib/utils";
import type { ProviderMeta } from "@/lib/providers";

/**
 * 渲染一个 provider 的图标，按 `colorLogo` 决定是否着色。
 *
 * 这条规则必须只有一处实现：原色 logo 的 `fill` 是硬编码的品牌色，给容器设
 * `color` **无效**（`fill` 优先），所以不能干预；而单色 logo 是
 * `fill="currentColor"`，不着色就会渲染成 `text-*` 继承来的黑/白，丢掉品牌色。
 * 两类图标在三个调用点各判一次必然漂移，所以收在这里。
 */
export function ProviderGlyph({
  meta,
  className,
}: {
  meta: ProviderMeta;
  className?: string;
}) {
  const Icon = meta.icon;
  return (
    <Icon className={className} style={meta.colorLogo ? undefined : { color: meta.color }} />
  );
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
 * 改造前这里是「品牌色实底方块 + 白色 glyph」；换成官方原色 logo 后那套不能
 * 用了——彩色 logo 压在品牌色实底上不可读。所以表面改为中性的
 * `bg-background` + `border-border`，深浅两套主题都由现有 token 自动成立：
 *
 * - 浅色模式下 chip 是白的，衬在侧栏（`--sidebar`）、侧栏 hover（`--hover`）、
 *   向导卡片（`--panel`）上都能看出来；衬在已展开的连接行（`--raised`，同为
 *   白色）上则靠 `border-border` 勾出轮廓。
 * - 暗色模式下 chip 比 `--raised` / `--panel` 更深，同样有边框兜底。
 *
 * `text-foreground` 不是装饰：[`AwsIcon`] 的 "aws" 字标用 `currentColor`，
 * 靠它跟着主题翻色（浅色深字 / 暗色浅字）。少了它字标会继承到别处的颜色。
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
        "flex shrink-0 items-center justify-center border border-border bg-background text-foreground",
        s.chip,
        className,
      )}
    >
      <ProviderGlyph meta={meta} className={s.glyph} />
    </span>
  );
}
