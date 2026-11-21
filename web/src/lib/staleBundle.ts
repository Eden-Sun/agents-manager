/**
 * 前端換版偵測（2026-11-21）：daemon 換版後，開著的分頁還跑舊的 JS bundle。網頁自己比對「載入中的主 script」與
 * 伺服器現在的 `index.html` 指的主 script；不同＝已換版，請使用者重新開啟。純函式，給 hook 與測試用。
 */

/** 正式 build 的主 entry：`<script type="module" src="/assets/index-XXXX.js">`；dev 模式（vite serve）沒有 `/assets/`，回 null。 */
export function mainScriptFromHtml(html: string): string | null {
  for (const m of html.matchAll(/<script\b[^>]*>/gi)) {
    const tag = m[0]
    if (!/\btype\s*=\s*["']module["']/i.test(tag)) continue
    const src = /\bsrc\s*=\s*["'](\/assets\/[^"']+\.js)["']/i.exec(tag)
    if (src) return src[1]
  }
  return null
}

/** 目前頁面載入的主 script（DOM 上的 module script）；dev 模式回 null。 */
export function loadedMainScript(doc: Pick<Document, 'querySelector'>): string | null {
  const el = doc.querySelector('script[type="module"][src*="/assets/"]')
  return el?.getAttribute('src') ?? null
}

/** 兩邊都拿得到且不同才是換版；任何一邊拿不到（dev、抓取失敗）都不算。 */
export function isStaleBundle(loaded: string | null, served: string | null): boolean {
  return loaded !== null && served !== null && loaded !== served
}

/** 節流：距離上次查詢不到 `gapMs` 就不再查。 */
export function shouldCheck(now: number, lastCheckAt: number | null, gapMs = 30_000): boolean {
  return lastCheckAt === null || now - lastCheckAt >= gapMs
}

/** 節流與「收起」記在模組層：WS 重連與回到前景共用同一個 30 秒窗。 */
let lastCheckAt: number | null = null
let dismissedFor: string | null = null

/** 測試用：把模組層的節流與收起狀態歸零。 */
export function resetStaleBundleForTest(): void {
  lastCheckAt = null
  dismissedFor = null
}

/** 查伺服器現在的 index.html；換版且沒被收起過就回傳新 script 路徑。 */
export async function checkServedBundle(loaded: string | null): Promise<string | null> {
  if (loaded === null || !shouldCheck(Date.now(), lastCheckAt)) return null
  lastCheckAt = Date.now()
  try {
    const res = await fetch('/index.html', { cache: 'no-store' })
    if (!res.ok) return null
    const served = mainScriptFromHtml(await res.text())
    return isStaleBundle(loaded, served) && served !== dismissedFor ? served : null
  } catch {
    return null // 離線或伺服器重啟中：下一次再查
  }
}

/** 使用者收起這一版：之後同一版不再跳。 */
export function dismissStaleBundle(version: string): void {
  dismissedFor = version
}
