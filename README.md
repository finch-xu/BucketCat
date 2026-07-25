<p align="center">
  <img src="assets/logo.png" alt="BucketCat" width="176">
</p>

<h1 align="center">BucketCat</h1>

<p align="center">跨平台对象存储客户端</p>

---

## 功能特性

- **一站接入主流对象存储** — 内置 Amazon S3、Cloudflare R2、MinIO、阿里云 OSS、腾讯云 COS、Backblaze B2 预设，也支持接入任意 S3 兼容服务。

- **大文件分片并发传输** — 超过 16 MiB 的文件自动分片，分片大小随文件体积自适应，上传与下载均以多分片并发进行。

- **断点续传** — 传输进度以 checkpoint 原子落盘，下载中途写入独立的 `.bcpart` 暂存文件、全部分片到位后才原子改名；应用重启后可从断点继续，不必从头再来。

- **传输任务全程可控** — 暂停、继续、取消、重试随时可用，实时显示进度、速度与剩余时间，并可调整同时进行的任务数与单任务并发分片数。


## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Copyright 2026 虚拟世界的懒猫
