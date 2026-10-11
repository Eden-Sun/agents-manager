/**
 * Jev 帳本的 store（issue #1328）：徽章要用的那一份（最近 200 筆）。
 * 開畫面時讀一次；之後每次 `supervisor_changed` 重抓，但最短 60 秒一次。
 * Jev 沒開、帳本 API 不存在（舊 daemon）、或讀失敗（例如 403）＝`visible` 為 false：徽章完全不顯示、也不報錯。
 */
import { create } from 'zustand'
import { fetchJudgeShadow, type JudgeShadowRow } from '../api/judge'
import { loadDismissed, rememberDismissed } from '../lib/judgeBadge'

const MIN_GAP_MS = 60_000

interface JudgeLedgerState {
  rows: JudgeShadowRow[]
  /** Jev 開著而且帳本讀得到。 */
  visible: boolean
  dismissed: Set<string>
  lastFetchAt: number
  inFlight: boolean
  refresh: (opts?: { force?: boolean }) => Promise<void>
  dismiss: (id: string) => void
}

export const useJudgeLedger = create<JudgeLedgerState>((set, get) => ({
  rows: [],
  visible: false,
  dismissed: loadDismissed(),
  lastFetchAt: 0,
  inFlight: false,
  refresh: async (opts) => {
    const now = Date.now()
    if (get().inFlight) return
    if (!opts?.force && now - get().lastFetchAt < MIN_GAP_MS) return
    set({ inFlight: true, lastFetchAt: now })
    try {
      const shadow = await fetchJudgeShadow()
      set({ rows: shadow?.rows ?? [], visible: shadow !== null && shadow.enabled })
    } catch {
      // 讀失敗（403、網路、daemon 暫時不在）：徽章隱藏、不報錯；下一次事件到了會再試（節流仍然算）。
      set({ rows: [], visible: false })
    } finally {
      set({ inFlight: false })
    }
  },
  dismiss: (id) => set((s) => ({ dismissed: rememberDismissed(id, s.dismissed) })),
}))

/** 測試用：回到初始狀態（不碰 localStorage 以外的東西）。 */
export function resetJudgeLedgerForTest(): void {
  useJudgeLedger.setState({ rows: [], visible: false, lastFetchAt: 0, inFlight: false })
}
