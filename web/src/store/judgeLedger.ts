/**
 * Jev 帳本的 store（issue #1328）：徽章要用的那一份（最近 200 筆）。
 * 開畫面時讀一次；之後每次 `supervisor_changed` 重抓，但最短 60 秒一次。
 * 節流期間（或抓到一半）來的事件不丟掉：排一次補抓，補抓排在滿 60 秒那一刻（#1378）。
 * Jev 沒開、帳本 API 不存在（舊 daemon）、或讀失敗（例如 403）＝`visible` 為 false：徽章完全不顯示、也不報錯。
 */
import { create } from 'zustand'
import { fetchJudgeShadow, type JudgeShadowRow } from '../api/judge'
import { loadDismissed, rememberDismissed } from '../lib/judgeBadge'

const MIN_GAP_MS = 60_000

/** 排著的那一次補抓（同一時間最多一個）。 */
let trailing: ReturnType<typeof setTimeout> | null = null
/** 抓到一半又來了事件：抓完後補抓一次。 */
let pendingAfterFlight = false
/** 抓到一半被要求強制重抓（儲存 Jev）：抓完立刻再抓一次，不等 60 秒（#1426）。 */
let pendingForce = false
/** 最近一次 refresh 看到的時間；補抓的延遲從它算起（測試注入時就是注入的時間）。 */
let lastSeen = 0

/** 排一次補抓，在 `dueAt` 那一刻執行；已經排著就不疊。補抓也用 `dueAt` 當時間，不另外讀牆鐘。 */
function scheduleAt(dueAt: number): void {
  if (trailing !== null) return
  trailing = setTimeout(() => {
    trailing = null
    void useJudgeLedger.getState().refresh({ now: dueAt })
  }, Math.max(0, dueAt - lastSeen))
}

interface JudgeLedgerState {
  rows: JudgeShadowRow[]
  /** Jev 開著而且帳本讀得到。 */
  visible: boolean
  dismissed: Set<string>
  lastFetchAt: number
  inFlight: boolean
  refresh: (opts?: { force?: boolean; now?: number }) => Promise<void>
  dismiss: (id: string) => void
}

export const useJudgeLedger = create<JudgeLedgerState>((set, get) => ({
  rows: [],
  visible: false,
  dismissed: loadDismissed(),
  lastFetchAt: 0,
  inFlight: false,
  refresh: async (opts) => {
    const now = opts?.now ?? Date.now()
    lastSeen = now
    if (get().inFlight) {
      // 強制重抓不能被「抓取中」吞掉：記下來，抓完立刻做（#1426）。一般事件只排節流後的補抓。
      if (opts?.force) pendingForce = true
      else pendingAfterFlight = true
      return
    }
    if (!opts?.force && now - get().lastFetchAt < MIN_GAP_MS) {
      scheduleAt(get().lastFetchAt + MIN_GAP_MS)
      return
    }
    // 這一趟就是補抓或強制重抓：排著的那一次不需要了。
    if (trailing !== null) {
      clearTimeout(trailing)
      trailing = null
    }
    set({ inFlight: true, lastFetchAt: now })
    try {
      const shadow = await fetchJudgeShadow()
      set({ rows: shadow?.rows ?? [], visible: shadow !== null && shadow.enabled })
    } catch {
      // 讀失敗（403、網路、daemon 暫時不在）：徽章隱藏、不報錯；下一次事件到了會再試（節流仍然算）。
      set({ rows: [], visible: false })
    } finally {
      set({ inFlight: false })
      if (pendingForce) {
        pendingForce = false
        pendingAfterFlight = false
        void get().refresh({ force: true, now: lastSeen })
      } else if (pendingAfterFlight) {
        pendingAfterFlight = false
        scheduleAt(get().lastFetchAt + MIN_GAP_MS)
      }
    }
  },
  dismiss: (id) => set((s) => ({ dismissed: rememberDismissed(id, s.dismissed) })),
}))

/** 測試用：回到初始狀態（不碰 localStorage 以外的東西）；排著的補抓一起取消。 */
export function resetJudgeLedgerForTest(): void {
  if (trailing !== null) clearTimeout(trailing)
  trailing = null
  pendingAfterFlight = false
  pendingForce = false
  lastSeen = 0
  useJudgeLedger.setState({ rows: [], visible: false, lastFetchAt: 0, inFlight: false })
}
