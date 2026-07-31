#!/usr/bin/env node
// 用 release 里上传的 .sig 文件 + tag 拼出 Tauri updater 要求的 latest.json。
//
// 用法: node scripts/generate-latest-json.mjs <tag> <sig-dir> <out-file>
// 环境变量:
//   GH_TOKEN - gh CLI 鉴权 (workflow 里是 secrets.GITHUB_TOKEN)
//   GH_REPO  - owner/repo，用来拼下载 URL
//
// 平台 key 由 .sig 的文件名推断，所以 workflow 里那步"把产物改成不含版本号的固定
// 名字"是这个脚本的前提，不是可有可无的美化。

import { readFileSync, writeFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";
import { execFileSync } from "node:child_process";

const [, , tag, sigDir, outFile] = process.argv;
if (!tag || !sigDir || !outFile) {
  console.error("用法: generate-latest-json.mjs <tag> <sig-dir> <out-file>");
  process.exit(1);
}

const repo = process.env.GH_REPO;
if (!repo) {
  console.error("GH_REPO 环境变量未设置");
  process.exit(1);
}

const version = tag.startsWith("v") ? tag.slice(1) : tag;

// release notes 取草稿 release 的 body。常常是空的（草稿刚由构建 job 建出来、还没
// 写正文），所以取不到只是 warn：更新面板对空 notes 有降级渲染，为此让整条发布链路
// 失败并不划算。
let notes = "";
try {
  notes = execFileSync(
    "gh",
    ["release", "view", tag, "--repo", repo, "--json", "body", "--jq", ".body"],
    { encoding: "utf8" },
  ).trim();
} catch (e) {
  console.warn("无法获取 release body，notes 留空:", e.message);
}

/** 从 updater 产物文件名推断 Tauri 的平台 key，认不出返回 null。 */
function platformKeyFor(baseName) {
  if (baseName.endsWith(".app.tar.gz")) {
    if (baseName.includes("macOS-arm64")) return "darwin-aarch64";
    if (baseName.includes("macOS-x64")) return "darwin-x86_64";
    return null;
  }
  // Tauri 2 直接对 NSIS 安装器 .exe 签名，不再产出 v1 时代的 .nsis.zip。
  // 两个架构的 .exe 必须靠文件名区分，否则会撞进同一个 key 互相覆盖。
  if (baseName.endsWith(".exe")) {
    if (baseName.includes("windows-arm64")) return "windows-aarch64";
    if (baseName.includes("windows-x64")) return "windows-x86_64";
    return null;
  }
  if (baseName.endsWith(".AppImage")) return "linux-x86_64";
  return null;
}

const files = readdirSync(sigDir);
console.log("发现 sig 文件:", files);

const platforms = {};
for (const sigName of files) {
  if (!sigName.endsWith(".sig")) continue;
  const baseName = sigName.slice(0, -4);
  const key = platformKeyFor(baseName);
  if (!key) {
    console.warn("跳过无法识别平台的 sig:", baseName);
    continue;
  }
  if (platforms[key]) {
    // 静默覆盖会发出一份指向错误架构二进制的 manifest —— 用户装上去直接跑不起来。
    throw new Error(
      `平台 ${key} 收到了两个产物 (后一个是 ${baseName})，检查产物改名步骤`,
    );
  }
  platforms[key] = {
    signature: readFileSync(resolve(sigDir, sigName), "utf8").trim(),
    url: `https://github.com/${repo}/releases/download/${tag}/${baseName}`,
  };
}

// 缺任何一个平台都直接失败，不发半份 manifest。Tauri 在比对版本号之前会先校验整个
// 文件，所以一份缺项的 manifest 不只是"少一个平台"——缺失平台上的用户会拿到一个
// 检查失败，而不是"已是最新版本"。
const REQUIRED = [
  "darwin-aarch64",
  "darwin-x86_64",
  "windows-x86_64",
  "windows-aarch64",
  "linux-x86_64",
];
const missing = REQUIRED.filter((k) => !platforms[k]);
if (missing.length > 0) {
  throw new Error(
    `缺少平台 ${missing.join(", ")} 的 .sig —— 检查对应 build job 是否成功、` +
      `TAURI_SIGNING_PRIVATE_KEY 是否注入`,
  );
}

const manifest = {
  version,
  notes,
  pub_date: new Date().toISOString(),
  platforms,
};

writeFileSync(outFile, JSON.stringify(manifest, null, 2));
console.log(`写入 ${outFile}:`);
console.log(JSON.stringify(manifest, null, 2));
