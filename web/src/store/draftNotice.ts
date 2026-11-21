import { create } from 'zustand'
import { ApiError } from '../api/types'
import { t } from '../lib/i18n'

interface DraftNoticeState {
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

export function draftMaxBytesOf(error: unknown): number | null {
  if (!(error instanceof ApiError)) return null
  const n = error.body.max_bytes
  return typeof n === 'number' && Number.isInteger(n) && n > 0 ? n : null
}

export function draftLimitText(bytes: number): string {
  return `${Math.round(bytes / 1024)} KB`
}

export function draftTooLargeText(maxBytes: number | null): string {
  return maxBytes === null
    ? t('這份草稿太長，沒有同步到其他裝置；內容還留在這個輸入框裡。')
    : t('這份草稿太長（上限 {limit}），沒有同步到其他裝置；內容還留在這個輸入框裡。', { limit: draftLimitText(maxBytes) })
}
