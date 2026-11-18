/**
 * bot 給的 .html 的沙盒預覽（issue #1868 註解預告的設計，使用者 2026-11-18）。
 *
 * 為什麼不能直接開：預覽分頁是同源的 blob: URL，伺服器的 CSP 管不到它，bot 產的 html 直接開就等於在主應用同源執行
 * （讀得到 localStorage／token、呼叫得到 API）。所以 html 一律放進 `<iframe sandbox="allow-scripts" srcdoc=…>`：
 * 沒有 allow-same-origin，iframe 是 opaque origin——能跑自己的 JS（圖表等），讀不到主應用的 storage／cookie／DOM，
 * 不能導走上層（沒有 allow-top-navigation）、不能開視窗（沒有 allow-popups／allow-modals）、不能送表單（沒有 allow-forms）。
 *
 * 外殼頁只有我們自己的靜態程式碼，**沒有任何 bot 內容、沒有 script**：由父頁在開窗後用 DOM API 建 iframe 並設 `srcdoc`
 * （不拼字串進外殼 HTML）。外殼帶 CSP meta；srcdoc 文件會繼承它，所以同時是 bot html 的上限：不能連網（`connect-src 'none'`、
 * 沒有外部 script／樣式／圖片，只有內嵌與 data:／blob:），圖表要用內嵌的程式與資料。
 */

/** iframe 的 sandbox 旗標：剛好這一個。測試鎖死，之後要加旗標必須改這裡與測試。 */
export const SANDBOX_FLAGS = 'allow-scripts'

/** 外殼頁的 CSP（srcdoc 的 bot html 繼承它）。 */
export const SHELL_CSP = [
  "default-src 'none'",
  "script-src 'unsafe-inline' 'unsafe-eval'",
  "style-src 'unsafe-inline'",
  'img-src data: blob:',
  'font-src data:',
  'media-src data: blob:',
  "connect-src 'none'",
  "frame-src about: data: blob:",
  "form-action 'none'",
  "base-uri 'none'",
].join('; ')

/** 外殼頁的靜態 HTML（沒有 bot 內容、沒有 script、標題與 iframe 都由 DOM 補）。 */
export const SHELL_HTML =
  `<!doctype html><html><head><meta charset="utf-8">` +
  `<meta http-equiv="Content-Security-Policy" content="${SHELL_CSP}">` +
  `<title></title>` +
  `<style>html,body{margin:0;height:100%;background:#fff}iframe{display:block;border:0;width:100%;height:100%}</style>` +
  `</head><body></body></html>`

/** 只有 .html／.htm 走沙盒預覽；不改 INLINE_PREVIEW_EXTENSIONS 的語意（那條是「能直接開」的白名單）。svg 不在範圍。 */
export function isSandboxPreviewable(name: string): boolean {
  return /\.html?$/i.test(name)
}

/** 在已開好的空白分頁裡建外殼：標題是檔名，iframe 佔滿視窗，bot 的 html 用 DOM 設進 `srcdoc`。 */
export function renderSandboxShell(w: Window, name: string, html: string): void {
  const doc = w.document
  doc.open()
  doc.write(SHELL_HTML)
  doc.close()
  doc.title = name
  const frame = doc.createElement('iframe')
  frame.setAttribute('sandbox', SANDBOX_FLAGS)
  frame.setAttribute('referrerpolicy', 'no-referrer')
  frame.srcdoc = html
  doc.body.appendChild(frame)
}
