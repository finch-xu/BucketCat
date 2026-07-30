import type { CSSProperties, ComponentType } from "react";
import { Box, CloudRain } from "lucide-react";
import {
  AlibabaCloudIcon,
  AwsIcon,
  BackblazeIcon,
  CloudflareIcon,
  MinioIcon,
  QiniuIcon,
  TencentCloudIcon,
} from "@/components/icons/providers";

/**
 * Static metadata for the object-storage providers BucketCat knows about --
 * icon, brand color, and default endpoint/region used to prefill the
 * add-connection wizard. This is real reference data (not a fixture); the
 * sidebar maps a saved connection's `provider` string (see `ProviderKind` in
 * `@/lib/api`) back to this table for its icon/color, falling back to the
 * `generic` entry for unknown ids.
 */

/** 图标组件的最小契约：接受 `className` 与 `style`。lucide 的 `LucideIcon`
 * 与 `@/components/icons/providers` 里内联的品牌 SVG 组件都满足它，所以这张
 * 表可以同时装两类图标。 */
export type IconComponent = ComponentType<{
  className?: string;
  style?: CSSProperties;
}>;

export interface ProviderMeta {
  id: string;
  name: string;
  nameKey?: string;
  descKey: string;
  /** 品牌色。**仅**用于给单色图标着色（`colorLogo === false`）——
   * 详见 `ProviderGlyph`。原色 logo 自带配色，此值对它们不生效，留在这里
   * 只是为了让每个 provider 都有一个可查的品牌色。 */
  color: string;
  icon: IconComponent;
  /** `icon` 是否自带品牌配色。`true` = 原色 logo，渲染时不得覆盖颜色。 */
  colorLogo: boolean;
  endpoint: string;
  region: string;
}

/**
 * 图标来源规则（改这张表时请一并遵守）：
 * - 有官方品牌 logo 的厂商 → 用 `@/components/icons/providers` 里内联的原色
 *   logo，`colorLogo: true`。
 * - **没有**官方品牌 logo 的厂商 → 用 lucide 线性图标 + `color` 着色，
 *   `colorLogo: false`。目前只有两个：`rainyun`（雨云官网首页、CSS/JS
 *   bundle 和 app 控制台都查过，`favicon.ico` 是一个 64×64 的 🌦 emoji 位图，
 *   不是品牌标；`CloudRain` 反而语义贴切）和 `generic`（本就没有厂商）。
 * - `minio` / `b2` 用的是 simple-icons 的单色 logo，所以也是
 *   `colorLogo: false`，靠这里的官方品牌色着色。
 */
export const PROVIDERS: ProviderMeta[] = [
  { id: "s3", name: "Amazon S3", descKey: "providers.s3", color: "#E67C29", icon: AwsIcon, colorLogo: true, endpoint: "https://s3.amazonaws.com", region: "us-east-1" },
  // R2's endpoint is built from the account id (see `R2Fields` / `src/lib/r2.ts`),
  // so there is no fixed default to prefill: the empty string keeps the form
  // from seeding a host the user would have to edit by hand. `R2Fields` renders
  // its own read-only, derived endpoint field and never reads this value.
  { id: "r2", name: "Cloudflare R2", descKey: "providers.r2", color: "#F6821F", icon: CloudflareIcon, colorLogo: true, endpoint: "", region: "auto" },
  { id: "minio", name: "MinIO", descKey: "providers.minio", color: "#C72E49", icon: MinioIcon, colorLogo: false, endpoint: "https://minio.local:9000", region: "us-east-1" },
  { id: "oss", name: "Aliyun OSS", descKey: "providers.oss", color: "#FF6A00", icon: AlibabaCloudIcon, colorLogo: true, endpoint: "https://oss-cn-hangzhou.aliyuncs.com", region: "cn-hangzhou" },
  { id: "rainyun", name: "Rainyun ROS", descKey: "providers.rainyun", color: "#3E8BD6", icon: CloudRain, colorLogo: false, endpoint: "https://cn-nb1.rains3.com", region: "cn-nb1" },
  { id: "qiniu", name: "Qiniu Kodo", descKey: "providers.qiniu", color: "#06AEEF", icon: QiniuIcon, colorLogo: true, endpoint: "https://s3.cn-east-1.qiniucs.com", region: "cn-east-1" },
  { id: "cos", name: "Tencent COS", descKey: "providers.cos", color: "#006EFF", icon: TencentCloudIcon, colorLogo: true, endpoint: "https://cos.ap-guangzhou.myqcloud.com", region: "ap-guangzhou" },
  { id: "b2", name: "Backblaze B2", descKey: "providers.b2", color: "#E21E29", icon: BackblazeIcon, colorLogo: false, endpoint: "https://s3.us-west-004.backblazeb2.com", region: "us-west-004" },
  { id: "generic", name: "", nameKey: "providers.genericName", descKey: "providers.generic", color: "#7d90a0", icon: Box, colorLogo: false, endpoint: "https://", region: "" },
];

/** Looks up display metadata for a saved connection's `provider` string,
 * falling back to the generic entry for ids this build doesn't recognize
 * (e.g. saved by a newer/older version). */
export function providerMeta(id: string): ProviderMeta {
  return (
    PROVIDERS.find((p) => p.id === id) ??
    PROVIDERS.find((p) => p.id === "generic") ??
    PROVIDERS[0]
  );
}
