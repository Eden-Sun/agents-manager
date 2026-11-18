/**
 * bot 給的 .html／.htm 的預覽（使用者 2026-11-18 明確決定：「.html 預覽不需要沙箱化」）。
 *
 * 取代 f443d9493 的 sandbox iframe：使用者要的是能正常打開 bot 產的報表（含 CDN 的 Chart.js 等外部 script／樣式／圖片），
 * 所以 ↗ 直接在新分頁打開完整頁面——帶 token 取檔、轉成 `text/html` 的 blob URL、`window.open` 先開空白頁（opener=null）再導過去、
 * 分頁關閉後 revoke，跟其他預覽同一條流程；不包 iframe、不加 CSP。
 * 風險已被接受：blob URL 與主應用同源，bot 產的 html 會以同源身分執行（讀得到 storage／token、打得到 API）。
 * 後端不變（.html 仍 octet-stream、`inline` 模式絕不回 text/html）；blob 的 type 由前端決定。
 */

/** 只有 .html／.htm；不改 INLINE_PREVIEW_EXTENSIONS 的語意（那條是後端 inline 白名單）。svg 不在範圍。 */
export function isHtmlFile(name: string): boolean {
  return /\.html?$/i.test(name)
}
