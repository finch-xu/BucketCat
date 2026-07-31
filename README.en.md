<p align="center">
  <img src="assets/logo.png" alt="BucketCat" width="176">
</p>

<h1 align="center">BucketCat</h1>

<p align="center">A simple, beautiful object storage client for macOS, Windows, and Linux</p>

<p align="center"><a href="README.md">简体中文</a> · <b>English</b> · <a href="https://deepwiki.com/finch-xu/BucketCat">DeepWiki</a></p>

---

## Features

- **One client for every major provider** — Nine built-in provider presets (see [Supported object storage](#supported-object-storage) below), plus any S3-compatible service you point it at.

- **Concurrent multipart transfers** — Files over 16 MiB are split automatically, with part size scaling to the file, and uploads and downloads both run several parts at once.

- **Resumable transfers** — Progress is checkpointed atomically to disk, and downloads land in a separate `.bcpart` staging file that is only renamed into place once every part has arrived. Restart the app and transfers pick up where they left off instead of starting over.

- **Transfers stay under your control** — Pause, resume, cancel, and retry are always available. Progress, speed, and time remaining update live, and you can tune how many tasks run at once and how many parts each task transfers in parallel.


## Supported object storage

Pick a provider when creating a connection and the endpoint and region are filled in for you — all that's left is your access key and secret key.

| Provider | Notes |
| --- | --- |
| Amazon S3 | |
| Cloudflare R2 | Enter your account ID; the endpoint is derived from it |
| MinIO | Self-hosted — point it at your own server |
| RustFS | Self-hosted — point it at your own server |
| Aliyun OSS | Endpoint follows the region you choose |
| Tencent COS | One connection covers one region; add a separate connection per region |
| Qiniu Kodo | Endpoint follows the region you choose |
| Rainyun ROS | Endpoint follows the region you choose |
| Backblaze B2 | Requires an application key, not the master key; the region is detected for you |
| Generic S3-compatible | Anything not listed above — enter the endpoint and region by hand |

## Download and install

Grab the installer for your platform from the [Releases](https://github.com/finch-xu/BucketCat/releases/latest) page.

**macOS** — `bucketcat_macOS-arm64.dmg` for Apple Silicon, `bucketcat_macOS-x64.dmg` for Intel. Open the dmg and drag BucketCat into Applications. Builds are signed and notarized, so Gatekeeper lets them through.

**Windows** — `bucketcat_windows-x64-setup.exe` (or `bucketcat_windows-arm64-setup.exe` on ARM devices); run it to install. An `.msi` is also published for x64. Builds are not code-signed yet, so SmartScreen will warn on first run — click **More info** → **Run anyway**.

**Linux** — `chmod +x` the `bucketcat_linux-x64.AppImage` and run it directly. A `bucketcat_linux-x64.deb` is available too.

Once installed, BucketCat checks for updates once at startup. A new version shows up as a small dot on the settings entry — nothing pops up and nothing downloads on its own. You can check manually and install with one click under **Settings › Update**. (If you installed the `.deb`, update through your package manager instead.)

## Development

You'll need Node 22+, pnpm 9+, and Rust 1.94.1+. On Linux, also install `libwebkit2gtk-4.1-dev`, `librsvg2-dev`, `libayatana-appindicator3-dev`, and `patchelf`.

```bash
pnpm install          # install frontend dependencies
pnpm tauri dev        # run in development (frontend HMR + Rust backend)
pnpm test             # frontend unit tests (Vitest)
pnpm tauri build      # build installers locally
```

Backend tests live in `src-tauri` — run `cargo test` there. The per-provider e2e suites are skipped by default because they need real accounts or a local container; each `src-tauri/tests/*_e2e.rs` explains how to run it in the comment at the top of the file.

Releases are built by GitHub Actions. Run `pnpm version:set <version>` to sync the version across package.json, Cargo.toml, and tauri.conf.json, then commit and push a `v<version>` tag.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Copyright 2026 虚拟世界的懒猫
