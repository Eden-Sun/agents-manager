/**
 * 輸入框草稿被 daemon 拒收（太大，PUT 回 400／413）時，輸入框上方的說明（#1656）。
 *
 * 草稿內容照舊留在本機輸入框；說明在送成功、別台寫入新版本、或 bot 被刪時收掉（由 `DraftSync` 的 `onRejectedCleared` 通知）。
 * 上限數字取 daemon 回應裡的 `max_bytes`，沒帶（舊 daemon、或 413 來自 HTTP 層）就不寫數字，不在 web 寫死一個可能跟 daemon 不同步的值。
 */
import { create } from 'zustand'
import { ApiError } from '../api/types'

interface DraftNoticeState {
  /** 目前被拒收的草稿 key → daemon 給的上限（位元組；沒給就是 null）。 */
  rejected: Record<string, number | null>
  mark: (key: string, error: unknown) => void
  clear: (key: string) => void
}

export const useDraftNotice = create<DraftNoticeState>((set) => ({
  rejected: {},
  mark: (key, error) => set((s) => ({ rejected: { ...s.rejected, [key]: draftMaxBytesOf(error) } })),
  clear: (key) =>
    set((s) => {
      if (!(key in s.rejected)) return s
      const rest = { ...s.rejected }
      delete rest[key]
      return { rejected: rest }
    }),
}))

/** daemon 的 400 `draft_too_large` 帶的 `max_bytes`；拿不到就 null。 */
export function draftMaxBytesOf(error: unknown): number | null {
  if (!(error instanceof ApiError)) return null
  const n = error.body.max_bytes
  return typeof n === 'number' && Number.isInteger(n) && n > 0 ? n : null
}

export function draftLimitText(bytes: number): string {
  return `${Math.round(bytes / 1024)} KB`
}

export function draftTooLargeText(maxBytes: number | null): string {
  const limit = maxBytes === null ? '' : `（上限 ${draftLimitText(maxBytes)}）`
  return `這份草稿太長${limit}，沒有同步到其他裝置；內容還留在這個輸入框裡。`
}
