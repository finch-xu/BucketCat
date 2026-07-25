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
});
