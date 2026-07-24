import { describe, expect, it } from "vitest";
import { previewKind, TEXT_PREVIEW_MAX } from "./preview";

describe("previewKind", () => {
  it("classifies images by extension, case-insensitively", () => {
    expect(previewKind("a.png", 1)).toBe("image");
    expect(previewKind("a.PNG", 1)).toBe("image");
    expect(previewKind("a.jpg", 1)).toBe("image");
    expect(previewKind("a.jpeg", 1)).toBe("image");
    expect(previewKind("a.gif", 1)).toBe("image");
    expect(previewKind("a.webp", 1)).toBe("image");
    expect(previewKind("a.svg", 1)).toBe("image");
    expect(previewKind("a.ico", 1)).toBe("image");
    expect(previewKind("a.bmp", 1)).toBe("image");
  });

  it("classifies video by extension", () => {
    expect(previewKind("a.mp4", 1)).toBe("video");
    expect(previewKind("a.webm", 1)).toBe("video");
    expect(previewKind("a.mov", 1)).toBe("video");
  });

  it("classifies audio by extension", () => {
    expect(previewKind("a.mp3", 1)).toBe("audio");
    expect(previewKind("a.wav", 1)).toBe("audio");
    expect(previewKind("a.ogg", 1)).toBe("audio");
    expect(previewKind("a.m4a", 1)).toBe("audio");
  });

  it("text only under the size cap", () => {
    for (const ext of [
      "txt",
      "md",
      "json",
      "xml",
      "csv",
      "js",
      "ts",
      "tsx",
      "jsx",
      "css",
      "html",
      "yml",
      "yaml",
      "log",
    ]) {
      expect(previewKind(`a.${ext}`, 100)).toBe("text");
    }
  });

  it("text at or over the size cap is none", () => {
    expect(previewKind("a.json", TEXT_PREVIEW_MAX - 1)).toBe("text");
    expect(previewKind("a.json", TEXT_PREVIEW_MAX)).toBe("none");
    expect(previewKind("a.json", TEXT_PREVIEW_MAX + 1)).toBe("none");
  });

  it("unknown / extensionless is none", () => {
    expect(previewKind("a.bin", 1)).toBe("none");
    expect(previewKind("a.pdf", 1)).toBe("none");
    expect(previewKind("a.docx", 1)).toBe("none");
    expect(previewKind("noext", 1)).toBe("none");
  });
});
