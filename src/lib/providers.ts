import {
  Box,
  Cloud,
  Database,
  HardDrive,
  Server,
  type LucideIcon,
} from "lucide-react";

/**
 * Static metadata for the object-storage providers BucketCat knows about --
 * icon, brand color, and default endpoint/region used to prefill the
 * add-connection wizard. This is real reference data (not a fixture); the
 * sidebar maps a saved connection's `provider` string (see `ProviderKind` in
 * `@/lib/api`) back to this table for its icon/color, falling back to the
 * `generic` entry for unknown ids.
 */
export interface ProviderMeta {
  id: string;
  name: string;
  nameKey?: string;
  descKey: string;
  color: string;
  icon: LucideIcon;
  endpoint: string;
  region: string;
}

export const PROVIDERS: ProviderMeta[] = [
  { id: "s3", name: "Amazon S3", descKey: "providers.s3", color: "#E67C29", icon: Box, endpoint: "s3.amazonaws.com", region: "us-east-1" },
  { id: "r2", name: "Cloudflare R2", descKey: "providers.r2", color: "#F6821F", icon: Cloud, endpoint: "<account>.r2.cloudflarestorage.com", region: "auto" },
  { id: "minio", name: "MinIO", descKey: "providers.minio", color: "#C4203F", icon: Server, endpoint: "https://minio.local:9000", region: "us-east-1" },
  { id: "oss", name: "Aliyun OSS", descKey: "providers.oss", color: "#FF6A00", icon: Database, endpoint: "oss-cn-hangzhou.aliyuncs.com", region: "cn-hangzhou" },
  { id: "cos", name: "Tencent COS", descKey: "providers.cos", color: "#0B63F6", icon: Cloud, endpoint: "cos.ap-guangzhou.myqcloud.com", region: "ap-guangzhou" },
  { id: "b2", name: "Backblaze B2", descKey: "providers.b2", color: "#E21E29", icon: HardDrive, endpoint: "s3.us-west-004.backblazeb2.com", region: "us-west-004" },
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
