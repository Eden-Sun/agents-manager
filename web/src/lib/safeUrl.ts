/**
 * 放進 `<a href>`／`window.open` 之前的檢查：只收絕對的 http／https 網址，其餘（`javascript:`、`data:`、`vbscript:`、
 * `file:`、相對路徑、看不懂的）一律 `undefined`。
 *
 * 網址不是我們寫死的：有些是 bot／API 呼叫端自己填的（遠端入口的 `url`）、有些是從外部資料帶進來的。
 * `javascript:` 在 `<a href>` 一按就在 app 的 origin 執行——記憶體裡的 daemon token 跟著一起暴露。
 * 用 `URL` 解析而不是比字串開頭：前導空白、大小寫、藏在協定中間的 Tab／換行，瀏覽器都會幫忙正規化。
 */
export function safeHttpUrl(url: string | null | undefined): string | undefined {
  if (!url) return undefined
  try {
    const u = new URL(url)
    return u.protocol === 'http:' || u.protocol === 'https:' ? url.trim() : undefined
  } catch {
    return undefined
  }
}
