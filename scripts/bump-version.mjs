#!/usr/bin/env node
// 把版本号同步写进四个文件，发版第一步。
//
// 用法: node scripts/bump-version.mjs 0.2.0
//
// 为什么需要这个脚本: 版本号在 package.json / Cargo.toml / tauri.conf.json 里各存
// 一份，没有任何构建期注入把它们对齐。而 CI 生成的 latest.json 里的 version 来自
// git tag，二进制里的真实版本来自 tauri.conf.json —— 两者一旦不一致，manifest 会
// 声称有新版、装上去却还是老版本，app 就陷入"永远提示有更新"的死循环。
// .github/workflows/release.yml 的 verify-version job 是第二道防线，会在构建前
// 把 tag 和这三处逐一比对；这个脚本让它不容易被触发。

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const version = process.argv[2];
if (!version) {
  console.error("用法: node scripts/bump-version.mjs <version>   例如 0.2.0");
  process.exit(1);
}
// 故意只收 MAJOR.MINOR.PATCH: Tauri updater 比较版本时走 semver，而带 -beta.1 这类
// 预发布后缀的版本在"内置更新源只有一个正式通道"的当前设计下没有分发路径。
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  console.error(`版本号格式非法: ${version} (期望 MAJOR.MINOR.PATCH，如 0.2.0)`);
  process.exit(1);
}

/** 读文件 → 跑一次替换 → 写回。替换没命中就报错退出，绝不静默跳过某个文件。 */
function patch(relPath, pattern, replacement) {
  const path = join(root, relPath);
  const before = readFileSync(path, "utf8");
  const after = before.replace(pattern, replacement);
  if (after === before) {
    console.error(`✗ ${relPath}: 没有匹配到版本号字段，文件结构可能变了，请手动检查`);
    process.exit(1);
  }
  writeFileSync(path, after);
  console.log(`✓ ${relPath}`);
}

patch("package.json", /("version"\s*:\s*")[^"]+(")/, `$1${version}$2`);

patch("src-tauri/tauri.conf.json", /("version"\s*:\s*")[^"]+(")/, `$1${version}$2`);

// 只认行首的 `version = "..."`。依赖行都以 crate 名开头 (`tokio = { version = ... }`)，
// 所以行首锚点足够把 [package] 那一行和依赖区分开。
patch("src-tauri/Cargo.toml", /^version\s*=\s*"[^"]+"/m, `version = "${version}"`);

// Cargo.lock 里 bucketcat 自己的条目。cargo 下次构建时本会自动修，但先改掉能让
// CI 的工作区保持干净，也让 `cargo build --locked` 可用。
patch(
  "src-tauri/Cargo.lock",
  /(name = "bucketcat"\nversion = ")[^"]+(")/,
  `$1${version}$2`,
);

console.log(`\n版本号已同步为 ${version}。接下来:`);
console.log(`  git commit -am "chore: bump version to ${version}"`);
console.log(`  git tag v${version} && git push origin main --tags`);
