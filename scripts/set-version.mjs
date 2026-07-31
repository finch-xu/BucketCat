#!/usr/bin/env node
// 把版本号同步写进四个文件，发版第一步。
//
// 用法: pnpm version:set 0.2.0
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
  console.error("用法: pnpm version:set <X.Y.Z>   例如 pnpm version:set 0.2.0");
  process.exit(1);
}
// 故意只收 MAJOR.MINOR.PATCH，比 cc-router 的同名脚本严格（那边允许 -beta.1 这类
// 后缀）。差异是有意的: BucketCat 的内置更新源只有一条正式通道，发一个预发布版本
// 等于把它推给全部用户。等真加了 beta 通道再放开这里。
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  console.error(`版本号格式非法: ${version} (期望 MAJOR.MINOR.PATCH，如 0.2.0)`);
  console.error("预发布后缀暂不支持: 目前只有一条正式更新通道，没有分发预发布版的途径。");
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
// `\r?\n` 而不是 `\n`: 这个仓库没有 .gitattributes 强制换行风格，克隆到 Windows
// 上的工作区会是 CRLF，届时纯 `\n` 会匹配不上而误报"文件结构变了"。
patch(
  "src-tauri/Cargo.lock",
  /(name = "bucketcat"\r?\nversion = ")[^"]+(")/,
  `$1${version}$2`,
);

console.log(`\n版本号已同步为 ${version}。接下来:`);
console.log(`  git add -u`);
console.log(`  git commit -m "chore: bump version to ${version}"`);
console.log(`  git tag v${version}`);
console.log(`  git push && git push --tags`);
