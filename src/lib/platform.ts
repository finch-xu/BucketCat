/**
 * macOS detection, whose one and only job is to reserve room for the native
 * traffic lights at the top of the sidebar.
 *
 * `tauri.conf.json` sets macOS `titleBarStyle` to `Overlay`: the window gets
 * no title bar, content fills its whole height, and the traffic lights float
 * *over* the webview. That means the top-left of the sidebar is territory the
 * system already owns -- without an inset the logo would sit underneath those
 * buttons. Windows and Linux keep their native title bar (we deliberately do
 * not set `decorations: false`), so their window buttons live outside the
 * webview entirely and need no inset at all.
 *
 * Split into a pure function plus a constant so the detection itself is
 * directly testable under vitest's node environment, with no `navigator` to
 * fake.
 */
export function isMacUserAgent(ua: string): boolean {
  return ua.includes("Macintosh");
}

export const isMac =
  typeof navigator === "undefined" ? false : isMacUserAgent(navigator.userAgent);
