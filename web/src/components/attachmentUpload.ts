/**
 * 一張附件卡片從「壓縮 → 上傳（帶進度）→ 好了／失敗」的狀態（issue #435）。
 * 跟 React 無關的部分放這裡：reducer、單張的上傳流程、進度文字，測試不必開 DOM。
 */

import type { Attachment } from '../api/types'
import { isAbortError } from '../api/transport'
import type { UploadOptions } from '../api/transport'
import { MAX_BYTES } from '../store/shelf'

/** One file waiting to be sent: compressing, uploading, ready (has an id), or failed. */
export interface Pending {
  key: string
  /** 名稱｜大小｜修改時間，擋重複用。 */
  fp: string
  name: string
  /** 實際上傳的大小（圖片壓縮後的）。 */
  size: number
  /** 壓縮前的大小；沒壓（非圖片、GIF、壓了沒變小）就沒有。 */
  originalSize?: number
  /** An image is drawn as a thumbnail; anything else as an icon card. */
  isImage: boolean
  /** Local preview for an image; `''` for everything else (no blob held for nothing). */
  previewUrl: string
  /** 圖片還在瀏覽器裡壓縮，還沒開始傳。 */
  compressing: boolean
  /** 已送出的位元組（`size` 是分母）。 */
  loaded: number
  id: string | null
  error: string | null
  /** 失敗後能不能重試：超過上限的重試也一樣過不了，不給按。 */
  retryable: boolean
}

export type PendingAction =
  | { type: 'add'; item: Pending }
  | { type: 'compressed'; key: string; name: string; size: number }
  | { type: 'uploading'; key: string }
  | { type: 'progress'; key: string; loaded: number }
  | { type: 'uploaded'; key: string; id: string }
  | { type: 'failed'; key: string; error: string; retryable: boolean }
  | { type: 'retry'; key: string }
  | { type: 'remove'; key: string }
  | { type: 'clear' }

export type AddVerdict = 'accept' | 'duplicate' | 'too-large'

/**
 * 收一個檔之前的守門（issue #497）：已經收過的略過，不能壓又超過上限的擋下來，收下的才把指紋記進 `seen`。
 * 記指紋跟判斷寫在一起是故意的——**擋下來的不能記**：它沒有卡片，也就沒有 × 可以按
 * （只有 `remove` 會把指紋收回去），指紋一記下去，同一個檔之後再拖幾次都算「重複」，
 * 沒有卡片也沒有通知，使用者只看得到第一次那張。
 */
export function admitFile(seen: Set<string>, fp: string, size: number, canCompress: boolean): AddVerdict {
  if (seen.has(fp)) return 'duplicate'
  // 圖片先壓再比上限（`lib/imageCompress.ts`）：手機原圖超過上限，壓完多半放得下，壓完還超過才算失敗。
  if (size > MAX_BYTES && !canCompress) return 'too-large'
  seen.add(fp)
  return 'accept'
}

export function pendingReducer(items: Pending[], action: PendingAction): Pending[] {
  const patch = (key: string, f: (it: Pending) => Pending) => items.map((it) => (it.key === key ? f(it) : it))
  switch (action.type) {
    case 'add':
      return [...items, action.item]
    case 'compressed':
      return patch(action.key, (it) => ({ ...it, name: action.name, originalSize: it.size, size: action.size }))
    case 'uploading':
      return patch(action.key, (it) => ({ ...it, compressing: false, loaded: 0 }))
    case 'progress':
      return patch(action.key, (it) => ({ ...it, loaded: Math.min(it.size, Math.max(0, action.loaded)) }))
    case 'uploaded':
      return patch(action.key, (it) => ({ ...it, id: action.id, loaded: it.size }))
    case 'failed':
      return patch(action.key, (it) => ({ ...it, compressing: false, error: action.error, retryable: action.retryable }))
    case 'retry':
      return patch(action.key, (it) => ({ ...it, error: null, loaded: 0 }))
    case 'remove':
      return items.filter((it) => it.key !== action.key)
    case 'clear':
      return []
  }
}

/** 壓完還是超過上限：重試不會變，所以跟網路錯誤分開。 */
class TooLargeError extends Error {}

export interface UploadDeps {
  compress: (file: File) => Promise<File>
  upload: (file: File, opts: UploadOptions) => Promise<Attachment>
  dispatch: (action: PendingAction) => void
  /** 壓縮完的那份記下來，重試時直接傳、不再壓一次。 */
  onPrepared: (file: File) => void
  onError: (name: string, message: string) => void
  formatSize: (bytes: number) => string
}

/**
 * 一張卡片的一次上傳。`signal` 中止（按 × 移除、清空、換對話）就安靜收手：
 * 不記失敗、不跳通知——使用者自己拿掉的，不是出錯。
 */
export async function runUpload(deps: UploadDeps, key: string, file: File, compress: boolean, signal: AbortSignal): Promise<void> {
  try {
    let out = file
    if (compress) {
      out = await deps.compress(file)
      if (signal.aborted) return
      if (out.size > MAX_BYTES) {
        throw new TooLargeError(`有 ${deps.formatSize(out.size)}，超過 ${deps.formatSize(MAX_BYTES)} 上限`)
      }
      if (out !== file) deps.dispatch({ type: 'compressed', key, name: out.name, size: out.size })
      deps.onPrepared(out)
    }
    deps.dispatch({ type: 'uploading', key })
    const worthDrawing = progressGate()
    const a = await deps.upload(out, {
      signal,
      onProgress: (loaded, total) => {
        if (!signal.aborted && worthDrawing(loaded, total)) deps.dispatch({ type: 'progress', key, loaded })
      },
    })
    if (signal.aborted) return
    deps.dispatch({ type: 'uploaded', key, id: a.id })
  } catch (e: unknown) {
    if (signal.aborted || isAbortError(e)) return
    const msg = e instanceof Error ? e.message : String(e)
    deps.dispatch({ type: 'failed', key, error: msg, retryable: !(e instanceof TooLargeError) })
    deps.onError(file.name, msg)
  }
}

/**
 * 一次上傳的進度節流（issue #438）：**只有卡片上那行字真的會變時才值得 dispatch**。
 *
 * XHR 的上傳進度事件依規範最密約每 50 ms 一次（≈20 次／秒），而這張卡片的 state 住在
 * `ChatPanel`，所以每一次 dispatch 都會重繪整個聊天面板，包含沒有 memo 的訊息列
 * （`MessageList` 每次重繪都會對所有 turn 做一次 filter+sort）。50 MB 在慢網路上要好幾分鐘，
 * 那就是好幾千次重繪，而使用者看得到的變化只有 [`progressLabel`] 那行字。
 *
 * 用「畫出來的字」當閘門而不是固定時間或固定百分比：門檻自動跟著顯示精度走，
 * 之後有人把文案改成更細或更粗，節流也跟著對，不會出現「字會變但沒重繪」的落差。
 * 50 MB 的上限是 0.1 MB 與 1% 兩種精度裡較細的那個，約 500 階。
 */
export function progressGate(): (loaded: number, total: number) => boolean {
  let shown = ''
  return (loaded, total) => {
    const next = progressLabel(loaded, total)
    if (next === shown) return false
    shown = next
    return true
  }
}

/** 往下取整：送到 99.6% 不先寫成 100%。 */
export function uploadPercent(loaded: number, total: number): number {
  if (total <= 0) return 0
  return Math.max(0, Math.min(100, Math.floor((loaded * 100) / total)))
}

/** `3.2 / 12.6 MB · 25%`：單位跟著總大小走，兩個數字才比得起來。 */
export function progressLabel(loaded: number, total: number): string {
  const pct = uploadPercent(loaded, total)
  if (total < 1024) return `${loaded} / ${total} B · ${pct}%`
  if (total < 1024 * 1024) return `${Math.round(loaded / 1024)} / ${Math.round(total / 1024)} KB · ${pct}%`
  const mb = (n: number) => (n / (1024 * 1024)).toFixed(1)
  return `${mb(loaded)} / ${mb(total)} MB · ${pct}%`
}
