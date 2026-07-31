import type { CSSProperties, ComponentType } from "react";
import {
  Box,
  Boxes,
  Cloud,
  CloudRain,
  Container,
  Globe,
  HardDrive,
  Server,
  Warehouse,
} from "lucide-react";

/**
 * Static metadata for the object-storage providers BucketCat knows about --
 * icon, brand color, and default endpoint/region used to prefill the
 * add-connection wizard. This is real reference data (not a fixture); the
 * sidebar maps a saved connection's `provider` string (see `ProviderKind` in
 * `@/lib/api`) back to this table for its icon/color, falling back to the
 * `generic` entry for unknown ids.
 */

/** 图标组件的最小契约：接受 `className` 与 `style`。lucide 的 `LucideIcon`
 * 满足它。保留这个别名而不是直接用 `LucideIcon`，是为了让 `ProviderGlyph`
 * 的契约独立于图标库的具体类型。 */
export type IconComponent = ComponentType<{
  className?: string;
  style?: CSSProperties;
}>;

export interface ProviderMeta {
  id: string;
  name: string;
  nameKey?: string;
  descKey: string;
  /** 品牌色。用于给 `icon` 着色 —— 见 `ProviderGlyph`。 */
  color: string;
  icon: IconComponent;
  endpoint: string;
  region: string;
}

/**
 * 图标规则（改这张表时请务必遵守）：**一律用 lucide 的中性线性图标 + `color`
 * 着色，不得内联任何厂商的品牌 logo。**
 *
 * 这不是审美选择，是商标合规要求。2026-07-31 逐家核查各厂商的商标政策，
 * 结论是本项目这种「在软件界面里展示厂商图形 logo 以标识可连接的服务」的用法
 * 基本都不被允许：
 *
 * - **AWS**：logo 只能用于「AWS 事先批准的材料」，许可仅发给客户/合作伙伴。
 * - **Cloudflare**：明文「in **word form only** (word marks, not logos)」，
 *   使用 logo 需事先书面许可。
 * - **MinIO**：禁止「change the colors / change the proportions」，而单色化
 *   再着色两条都踩；另有「作 site icon 需书面许可」。
 * - **阿里云**：「未经许可，任何人不得擅自…展示」，且「相关图形、标识」明确涵盖。
 * - **腾讯云**：「未经腾讯云事先书面同意，您不得使用…商标、服务标志、
 *   服务或商品名称、标识」。
 * - **七牛云**：「在任何情况下均不得」，措辞是绝对化的封闭式禁止。
 * - **Backblaze**：查无任何公开商标政策（`/trademark`、`/brand` 均 404），
 *   素材包内也没有 LICENSE —— 无政策可依即按未授权处理。
 * - **RustFS**：是唯一明确授权「支持列表」用途的，但附带 ® 符号、归属声明、
 *   来源标注三项持续义务，且其政策页官网已 404（仅存 GitHub 副本）。
 *   为一家 provider 背这些不划算，所以同样走中性图标。
 *
 * **不要以为从 simple-icons / @lobehub/icons 取用就没问题。** 那些是版权许可，
 * 不是商标许可，也无权代厂商授予 —— CC0 1.0 §4(a) 白纸黑字排除商标，
 * simple-icons 自己的 DISCLAIMER 也要求使用者「seek the correct permissions」。
 * 收录 ≠ 授权。
 *
 * 厂商**名称文字**保留：文字形式的指明性使用是 AWS / Cloudflare / MinIO
 * 的政策都明确允许的（「for use with」「compatible with」这类引用），
 * 且不写厂商名的话产品就无法标识自己支持哪些服务。相应的声明见根目录 NOTICE。
 *
 * 选图时避开了三处应用内已占用的语义，别再撞回去：`Database` 是面包屑里的
 * 存储桶（`path-bar.tsx`），`Archive` 是压缩包文件类型（`file-meta.ts`），
 * `Folder` 是文件夹。撞了不会报错，只会让用户觉得两个东西为什么长一样。
 */
export const PROVIDERS: ProviderMeta[] = [
  { id: "s3", name: "Amazon S3", descKey: "providers.s3", color: "#E67C29", icon: Container, endpoint: "https://s3.amazonaws.com", region: "us-east-1" },
  // R2's endpoint is built from the account id (see `R2Fields` / `src/lib/r2.ts`),
  // so there is no fixed default to prefill: the empty string keeps the form
  // from seeding a host the user would have to edit by hand. `R2Fields` renders
  // its own read-only, derived endpoint field and never reads this value.
  { id: "r2", name: "Cloudflare R2", descKey: "providers.r2", color: "#F6821F", icon: Globe, endpoint: "", region: "auto" },
  { id: "minio", name: "MinIO", descKey: "providers.minio", color: "#C72E49", icon: Server, endpoint: "https://minio.local:9000", region: "us-east-1" },
  { id: "oss", name: "Aliyun OSS", descKey: "providers.oss", color: "#FF6A00", icon: Boxes, endpoint: "https://oss-cn-hangzhou.aliyuncs.com", region: "cn-hangzhou" },
  { id: "rainyun", name: "Rainyun ROS", descKey: "providers.rainyun", color: "#3E8BD6", icon: CloudRain, endpoint: "https://cn-nb1.rains3.com", region: "cn-nb1" },
  { id: "qiniu", name: "Qiniu Kodo", descKey: "providers.qiniu", color: "#06AEEF", icon: Cloud, endpoint: "https://s3.cn-east-1.qiniucs.com", region: "cn-east-1" },
  { id: "cos", name: "Tencent COS", descKey: "providers.cos", color: "#006EFF", icon: HardDrive, endpoint: "https://cos.ap-guangzhou.myqcloud.com", region: "ap-guangzhou" },
  { id: "b2", name: "Backblaze B2", descKey: "providers.b2", color: "#E21E29", icon: Warehouse, endpoint: "https://s3.us-west-004.backblazeb2.com", region: "us-west-004" },
  { id: "generic", name: "", nameKey: "providers.genericName", descKey: "providers.generic", color: "#7d90a0", icon: Box, endpoint: "https://", region: "" },
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
