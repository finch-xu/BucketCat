import { openUrl } from "@tauri-apps/plugin-opener";

/**
 * 在系统默认浏览器里打开一个外部链接。
 *
 * 全应用唯一的外链出口。不要用 `<a target=_blank>`（此处刻意不加引号：守卫测试
 * 扫的就是带引号的写法，加了引号这行注释自己就会被举报）：在 Tauri 的 webview 里
 * 它请求宿主再开一个 webview，wry 没有注册 handler，点击被静默丢弃；而去掉该
 * 属性会让裸 `href` 把应用自己的 webview 导航走且无法返回。
 * `src/lib/source-hygiene.test.ts` 里有守卫测试盯着这一点。
 *
 * 能打开哪些 URL 由 `src-tauri/capabilities/default.json` 的
 * `opener:allow-open-url` 白名单在构建期决定，不在这里做运行时校验 —— 白名单
 * 之外的地址会让下面的 `openUrl` reject。
 */
export async function openExternal(url: string): Promise<void> {
  try {
    await openUrl(url);
  } catch (err) {
    // 刻意只记日志、不向用户报错，和 about-pane 读版本号失败时的处理一致。
    // 剩下的失败来路都不是用户能补救的：URL 不在 capability 白名单里属于编码
    // 错误，而 `pnpm dev` 直连浏览器时根本没有 Tauri 运行时。真正的打包应用里
    // 这条路径不该被走到，走到了就去看控制台。
    console.error("Failed to open the external link", err);
  }
}
