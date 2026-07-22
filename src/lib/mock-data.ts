/**
 * Fixture object tree backing the M2 file-browser placeholder (real listing
 * lands in M3). Provider reference data lives in `@/lib/providers` now --
 * that's live metadata the app ships with, not a fixture, so it doesn't
 * belong alongside this mock tree.
 */
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
