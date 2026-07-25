import {
  Archive,
  Braces,
  Code,
  File,
  FileText,
  Folder,
  Image,
  Music,
  Type,
  Video,
  type LucideIcon,
} from "lucide-react";

export interface FileMeta {
  icon: LucideIcon;
  color: string;
  labelKey: string;
}

export const FOLDER_META: FileMeta = { icon: Folder, color: "#e0a44a", labelKey: "fileTypes.folder" };

const EXT_META: Record<string, FileMeta> = {
  png: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.png" },
  jpg: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.jpg" },
  jpeg: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.jpg" },
  gif: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.gif" },
  webp: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.webp" },
  ico: { icon: Image, color: "#5b9bd5", labelKey: "fileTypes.ico" },
  svg: { icon: Image, color: "#9b7fd4", labelKey: "fileTypes.svg" },
  mp4: { icon: Video, color: "#e0708a", labelKey: "fileTypes.mp4" },
  webm: { icon: Video, color: "#e0708a", labelKey: "fileTypes.webm" },
  mov: { icon: Video, color: "#e0708a", labelKey: "fileTypes.video" },
  mp3: { icon: Music, color: "#4bb39a", labelKey: "fileTypes.mp3" },
  wav: { icon: Music, color: "#4bb39a", labelKey: "fileTypes.audio" },
  html: { icon: Code, color: "#8a7de0", labelKey: "fileTypes.html" },
  css: { icon: Code, color: "#4aa3c4", labelKey: "fileTypes.css" },
  js: { icon: Braces, color: "#5aa86e", labelKey: "fileTypes.js" },
  json: { icon: Braces, color: "#5aa86e", labelKey: "fileTypes.json" },
  code: { icon: Code, color: "#7d90a0", labelKey: "fileTypes.xml" },
  md: { icon: FileText, color: "#8794a1", labelKey: "fileTypes.md" },
  txt: { icon: FileText, color: "#8794a1", labelKey: "fileTypes.txt" },
  csv: { icon: FileText, color: "#5aa86e", labelKey: "fileTypes.csv" },
  woff2: { icon: Type, color: "#c98a4a", labelKey: "fileTypes.font" },
  woff: { icon: Type, color: "#c98a4a", labelKey: "fileTypes.font" },
  ttf: { icon: Type, color: "#c98a4a", labelKey: "fileTypes.font" },
  zip: { icon: Archive, color: "#d0a24a", labelKey: "fileTypes.archive" },
  tar: { icon: Archive, color: "#d0a24a", labelKey: "fileTypes.archive" },
  gz: { icon: Archive, color: "#d0a24a", labelKey: "fileTypes.archive" },
  archive: { icon: Archive, color: "#d0a24a", labelKey: "fileTypes.archive" },
  pdf: { icon: File, color: "#d05a5a", labelKey: "fileTypes.pdf" },
};

const DEFAULT_META: FileMeta = { icon: File, color: "#8794a1", labelKey: "fileTypes.file" };

export function fileMeta(kind: "folder" | "file", ext: string): FileMeta {
  if (kind === "folder") return FOLDER_META;
  return EXT_META[ext] ?? DEFAULT_META;
}

const IMAGE_EXTS = new Set(["png", "jpg", "jpeg", "gif", "webp", "ico", "svg"]);

export function isImageExt(ext: string): boolean {
  return IMAGE_EXTS.has(ext);
}
