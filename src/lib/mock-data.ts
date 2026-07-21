import {
  Box,
  Cloud,
  Database,
  HardDrive,
  Server,
  type LucideIcon,
} from "lucide-react";

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

export interface Connection {
  id: string;
  provider: string;
  name: string;
  color: string;
  icon: LucideIcon;
  buckets: string[];
}

export const MOCK_CONNECTIONS: Connection[] = [
  { id: "r2", provider: "Cloudflare R2", name: "cdn-prod", color: "#F6821F", icon: Cloud, buckets: ["assets", "media", "backups"] },
  { id: "s3", provider: "Amazon S3", name: "app-prod", color: "#E67C29", icon: Box, buckets: ["app-uploads", "logs"] },
  { id: "minio", provider: "MinIO", name: "homelab", color: "#C4203F", icon: Server, buckets: ["dev-bucket"] },
  { id: "oss", provider: "Aliyun OSS", name: "oss-hangzhou", color: "#FF6A00", icon: Database, buckets: ["static-cn"] },
];

export interface ObjectEntry {
  name: string;
  kind: "folder" | "file";
  ext: string;
  size: string;
  modified: string;
}

const f = (
  name: string,
  kind: "folder" | "file",
  ext = "",
  size = "—",
  modified = "—",
): ObjectEntry => ({ name, kind, ext, size, modified });

export const MOCK_TREE: Record<string, ObjectEntry[]> = {
  "assets::": [
    f("images", "folder"),
    f("css", "folder"),
    f("fonts", "folder"),
    f("videos", "folder"),
    f("index.html", "file", "html", "12.4 KB", "2026-07-18 09:22"),
    f("app.bundle.js", "file", "js", "284 KB", "2026-07-18 09:22"),
    f("data.json", "file", "json", "48 KB", "2026-07-15 14:03"),
    f("sitemap.xml", "file", "code", "6.2 KB", "2026-07-10 11:40"),
    f("README.md", "file", "md", "3.1 KB", "2026-07-02 16:18"),
    f("favicon.ico", "file", "ico", "4.2 KB", "2026-06-28 08:00"),
    f("robots.txt", "file", "txt", "312 B", "2026-06-28 08:00"),
  ],
  "assets::images": [
    f("thumbnails", "folder"),
    f("hero-banner.png", "file", "png", "1.4 MB", "2026-07-18 09:20"),
    f("og-cover.jpg", "file", "jpg", "842 KB", "2026-07-18 09:20"),
    f("logo-mark.svg", "file", "svg", "11.8 KB", "2026-07-12 10:11"),
    f("avatar-01.png", "file", "png", "96 KB", "2026-07-11 13:52"),
    f("avatar-02.png", "file", "png", "88 KB", "2026-07-11 13:52"),
    f("texture-noise.png", "file", "png", "204 KB", "2026-07-08 19:30"),
  ],
  "assets::images/thumbnails": [
    f("hero-sm.png", "file", "png", "62 KB", "2026-07-18 09:20"),
    f("og-sm.jpg", "file", "jpg", "40 KB", "2026-07-18 09:20"),
  ],
  "assets::css": [
    f("app.css", "file", "css", "58 KB", "2026-07-18 09:22"),
    f("reset.css", "file", "css", "2.1 KB", "2026-05-01 12:00"),
    f("theme.css", "file", "css", "9.4 KB", "2026-07-16 15:44"),
  ],
  "assets::fonts": [
    f("NotoSansSC.woff2", "file", "woff2", "4.8 MB", "2026-04-20 10:00"),
    f("JetBrainsMono.woff2", "file", "woff2", "142 KB", "2026-04-20 10:00"),
  ],
  "assets::videos": [
    f("intro.mp4", "file", "mp4", "48 MB", "2026-07-01 08:30"),
    f("demo.webm", "file", "webm", "31 MB", "2026-07-01 08:30"),
  ],
  "media::": [
    f("podcasts", "folder"),
    f("cover.jpg", "file", "jpg", "1.1 MB", "2026-07-19 20:00"),
    f("episode-14.mp3", "file", "mp3", "58 MB", "2026-07-19 20:00"),
  ],
  "backups::": [
    f("daily", "folder"),
    f("backup-2026-07.tar.gz", "file", "archive", "318 MB", "2026-07-20 02:00"),
  ],
  "app-uploads::": [
    f("users", "folder"),
    f("receipt-8823.pdf", "file", "pdf", "208 KB", "2026-07-20 10:15"),
    f("export.csv", "file", "csv", "4.2 MB", "2026-07-19 22:00"),
  ],
  "logs::": [
    f("access.log", "file", "txt", "92 MB", "2026-07-21 00:00"),
    f("error.log", "file", "txt", "1.2 MB", "2026-07-21 00:00"),
  ],
  "dev-bucket::": [f("test.txt", "file", "txt", "12 B", "2026-07-21 09:00")],
};

export function treeKey(bucket: string, path: string[]): string {
  return bucket + "::" + path.join("/");
}
