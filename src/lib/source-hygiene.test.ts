import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

/** 递归收集 `src/` 下所有 TypeScript 源文件。 */
function sourceFiles(dir: string, acc: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) sourceFiles(full, acc);
    else if (/\.tsx?$/.test(entry)) acc.push(full);
  }
  return acc;
}

describe("source hygiene", () => {
  // 一个裸 NUL 字节会让 git 把整个文件当二进制处理（NUL 出现在前 8000
  // 字节内即触发），`git diff` 从此只输出 "Binary files ... differ"，
  // 代码审查流程对该文件彻底失效；ripgrep/ugrep 也会默认跳过它。
  // 需要 NUL 作哨兵值时用转义写法，运行时字符串完全等价。
  it("has no raw NUL bytes in any TypeScript source file", () => {
    const offenders = sourceFiles("src").filter((f) =>
      readFileSync(f).includes(0),
    );
    expect(offenders).toEqual([]);
  });

  // `target="_blank"` 在浏览器里正确、在 Tauri 的 webview 里静默失效：它请求
  // 宿主再开一个 webview，wry 没有注册这个 handler，点击被直接丢弃 —— 不报错、
  // 不跳转。曾经因此让「关于」和「更新」两个面板的外链同时点不动。去掉该属性
  // 也不是修法，裸 `href` 会把应用自己的 webview 导航到外部页面且无法返回。
  // 外链一律走 `lib/external-link.ts` 的 `openExternal`。
  it("has no anchor that asks the webview to open a new window", () => {
    // 本测试文件自身写有该模式，扫描时排除测试文件，否则它会举报自己。
    const offenders = sourceFiles("src")
      .filter((f) => !/\.test\.tsx?$/.test(f))
      .filter((f) => /target=["']_blank["']/.test(readFileSync(f, "utf8")));
    expect(offenders).toEqual([]);
  });
});
