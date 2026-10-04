/**
 * 分享頁的圖片：預覽、SVG 轉 PNG、手機分享（SPEC「分享 bot」§20 圖片）。
 *
 * 受限 bot 沒有 Bash，做圖卡只能寫 SVG；end user 要的是能存相簿、傳 LINE 的圖。所以在瀏覽器裡把 SVG 畫到
 * canvas 轉 PNG。安全上 SVG 一律經 `<img>`（blob: 或 `?inline=1` 的同源網址）或 canvas 繪製，**絕不**插進 DOM：
 * 當成圖片載入的 SVG 不跑 script、不載外部資源。
 */
import type { ShareFile } from './shareModel'

const IMAGE_RE = /\.(svg|png|jpe?g|gif|webp)$/i

export const isImageName = (name: string): boolean => IMAGE_RE.test(name)
export const isSvgName = (name: string): boolean => /\.svg$/i.test(name)
/** `早安.svg` → `早安.png`。 */
export const pngName = (name: string): string => `${name.replace(/\.svg$/i, '')}.png`

/**
 * 畫面上顯示的名字。分享頁的使用者是不懂電腦的長輩（使用者 2026-10-04）：圖片不顯示副檔名，
 * 也不讓她知道哪張是向量圖——她拿到的就是「一張圖」。
 */
export const displayName = (name: string): string => (isImageName(name) ? name.replace(IMAGE_RE, '') || name : name)

const at = (s: string | null | undefined): number => {
  const t = s ? Date.parse(s) : NaN
  return Number.isFinite(t) ? t : NaN
}

/** bot 寫檔與訊息落地之間的時鐘誤差（outbox 的時間只到秒）。 */
const SKEW_MS = 2000

/**
 * 每則 bot 回覆下面要直接畫哪些圖：
 * 1. 那個回合在 outbox 新增／更新的圖：以檔案時間找它之後的第一則訊息；那則是 bot 的就掛在那則，
 *    否則看檔案之前緊鄰的那則：是 bot 的＝回完話才寫的圖，掛在那則；是 end user 的＝這一回合 bot 還沒回，先不掛。
 *    同一張圖（照時間）只掛一次。
 * 2. bot 回覆文字裡提到的檔名（要真的在 outbox 裡）。
 * 每則最多 6 張。
 */
export function imagesByMessage(
  messages: readonly { id: string; role: string; text: string; created_at: string }[],
  files: readonly ShareFile[],
): Map<string, ShareFile[]> {
  const out = new Map<string, ShareFile[]>()
  const add = (id: string, f: ShareFile) => {
    const list = out.get(id) ?? []
    if (list.length < 6 && !list.some((x) => x.name === f.name)) list.push(f)
    out.set(id, list)
  }
  const imgs = files.filter((f) => isImageName(f.name))
  for (const f of imgs) {
    const t = at(f.modified_at)
    if (!Number.isFinite(t)) continue
    // bot 的回覆容許一點時鐘誤差；end user 的訊息要嚴格在檔案之後，才不會把這一回合的圖算到上一回合。
    const next = messages.findIndex((m) => (m.role === 'assistant' ? at(m.created_at) >= t - SKEW_MS : at(m.created_at) > t))
    if (next >= 0 && messages[next].role === 'assistant') {
      add(messages[next].id, f)
      continue
    }
    // 檔案之前緊鄰的那則是 bot 的＝bot 回完話才寫的圖；是 end user 的＝bot 這一回合還沒回，等回覆到了再掛。
    const prev = messages[(next < 0 ? messages.length : next) - 1]
    if (prev?.role === 'assistant') add(prev.id, f)
  }
  for (const m of messages) {
    if (m.role !== 'assistant') continue
    for (const f of imgs) if (m.text.includes(f.name)) add(m.id, f)
  }
  return out
}

/**
 * SVG 有沒有引用外部資源。`<img>` 裡的 SVG 根本不載外部資源（畫出來會缺一塊），`<foreignObject>` 在部分瀏覽器會
 * 汙染 canvas（`toBlob` 丟 SecurityError）。偵測到就只給 SVG 下載，不假裝能轉 PNG。寧可多擋：`#id` 與 `data:` 以外的
 * href／url()、`@import`、`<foreignObject>` 都算。看的是入口送來的內容：bot 引用資料夾裡的照片（`inbox/…`）daemon 已經
 * 嵌成 `data:`（SPEC §20.3），這裡只會看到嵌完仍留著的外部參照。
 */
export function svgExternalRefs(text: string): boolean {
  for (const m of text.matchAll(/\b(?:xlink:)?href\s*=\s*(["'])([^"']*)\1/gi)) {
    const v = m[2].trim()
    if (!v.startsWith('#') && !/^data:/i.test(v)) return true
  }
  if (/url\(\s*(["']?)\s*(?!#|data:)[^)\s"']/i.test(text)) return true
  return /@import\b/i.test(text) || /<foreignObject\b/i.test(text)
}

const num = (v: string | undefined): number | null => {
  if (!v) return null
  const m = v.trim().match(/^([0-9]*\.?[0-9]+)(px)?$/i)
  const n = m ? Number(m[1]) : NaN
  return Number.isFinite(n) && n > 0 ? n : null
}

/** 根 `<svg>` 的大小：先看 width／height（無單位或 px），沒有就用 viewBox。都沒有回 null。 */
export function svgSize(text: string): { w: number; h: number } | null {
  const tag = text.match(/<svg\b[^>]*>/i)?.[0]
  if (!tag) return null
  const attr = (n: string) => tag.match(new RegExp(`\\s${n}\\s*=\\s*["']([^"']*)["']`, 'i'))?.[1]
  const w = num(attr('width'))
  const h = num(attr('height'))
  const vb = attr('viewBox')
    ?.trim()
    .split(/[\s,]+/)
    .map(Number)
  const vw = vb && vb.length === 4 && vb[2] > 0 ? vb[2] : null
  const vh = vb && vb.length === 4 && vb[3] > 0 ? vb[3] : null
  if (w && h) return { w, h }
  if (w && vw && vh) return { w, h: (w * vh) / vw }
  if (h && vw && vh) return { w: (h * vw) / vh, h }
  if (vw && vh) return { w: vw, h: vh }
  return null
}

/** 沒有 width／height 的 SVG 補上（Firefox 畫不出沒有固有大小的 SVG）。只改根標籤的屬性，不碰內容。 */
export function svgWithSize(text: string, size: { w: number; h: number }): string {
  return text.replace(/<svg\b[^>]*>/i, (tag) => {
    let t = tag.replace(/\s(width|height)\s*=\s*(["'])[^"']*\2/gi, '')
    t = t.replace(/^<svg\b/i, `<svg width="${size.w}" height="${size.h}"`)
    return t
  })
}

/** 2x 解析度，但 canvas 單邊 ≤ 8192、總像素 ≤ 16M（iOS Safari 的上限，超過畫出來是空白）。 */
export function pngCanvasSize(w: number, h: number, scale = 2): { w: number; h: number } {
  const s = Math.min(scale, 8192 / w, 8192 / h, Math.sqrt(16_000_000 / (w * h)))
  return { w: Math.max(1, Math.floor(w * s)), h: Math.max(1, Math.floor(h * s)) }
}

/**
 * 是不是合法的 XML（bot 改圖寫壞，例如屬性之間少空格，瀏覽器整張畫不出來）。先問 `DOMParser`：比等 `<img>` 解碼失敗快，
 * 也不靠 `onerror` 一定會觸發。沒有 `DOMParser` 的環境當作合法，交給解碼那一關。
 */
export function svgWellFormed(text: string): boolean {
  if (typeof DOMParser === 'undefined') return true
  try {
    return new DOMParser().parseFromString(text, 'image/svg+xml').getElementsByTagName('parsererror').length === 0
  } catch {
    return false
  }
}

export class SvgTaintedError extends Error {
  constructor() {
    super('svg_tainted')
  }
}

/** SVG 原文 → PNG。經 blob: 網址的 `<img>` 畫到 canvas；背景照 SVG 原樣（透明就透明）。 */
export async function svgToPng(svgText: string, scale = 2): Promise<Blob> {
  if (svgExternalRefs(svgText)) throw new SvgTaintedError()
  const size = svgSize(svgText) ?? { w: 1080, h: 1080 }
  const url = URL.createObjectURL(new Blob([svgWithSize(svgText, size)], { type: 'image/svg+xml' }))
  try {
    const img = new Image()
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve()
      img.onerror = () => reject(new Error('svg_decode'))
      img.src = url
    })
    const out = pngCanvasSize(size.w, size.h, scale)
    const canvas = document.createElement('canvas')
    canvas.width = out.w
    canvas.height = out.h
    const ctx = canvas.getContext('2d')
    if (!ctx) throw new Error('no_canvas')
    ctx.drawImage(img, 0, 0, out.w, out.h)
    return await new Promise<Blob>((resolve, reject) => {
      try {
        canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('png_encode'))), 'image/png')
      } catch (e) {
        reject(e instanceof DOMException && e.name === 'SecurityError' ? new SvgTaintedError() : e)
      }
    })
  } finally {
    URL.revokeObjectURL(url)
  }
}

/** 存成檔案（桌機、或手機不支援分享時）。 */
export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = name
  a.rel = 'noopener'
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 30_000)
}

type ShareNav = Navigator & { canShare?: (d: { files: File[] }) => boolean }

/** 手機（觸控為主）而且瀏覽器能分享這個檔：才走系統分享選單（LINE、存到相簿）。桌機按「下載」不跳分享選單。 */
export function canShareFile(file: File): boolean {
  const nav = (typeof navigator === 'undefined' ? undefined : navigator) as ShareNav | undefined
  const coarse = typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia('(pointer: coarse)').matches
  return coarse && typeof nav?.share === 'function' && nav.canShare?.({ files: [file] }) === true
}

/**
 * 分享不了就下載。要在點擊的同一個 handler 裡呼叫（`navigator.share` 要使用者手勢；PNG 事先轉好，這裡不再等）。
 * 使用者自己取消分享不算失敗，也不改成下載。
 */
export async function shareOrSave(blob: Blob, name: string, type: string): Promise<'shared' | 'saved' | 'cancelled'> {
  const file = new File([blob], name, { type })
  if (canShareFile(file)) {
    try {
      await navigator.share({ files: [file] })
      return 'shared'
    } catch (e) {
      if (e instanceof DOMException && e.name === 'AbortError') return 'cancelled'
    }
  }
  saveBlob(blob, name)
  return 'saved'
}
