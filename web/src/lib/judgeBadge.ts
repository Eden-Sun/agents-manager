/**
 * bot 上的 Jev 徽章（issue #1328）：挑出該 bot 近 24 小時超過門檻、最新的那一筆（純函式），
 * 以及使用者「關掉這一則」的紀錄（每台瀏覽器各自，localStorage；讀寫都包 try/catch）。
 */
import type { JudgeShadowRow } from '../api/judge'
import { overThreshold } from './judgeStats'

const DAY_MS = 24 * 3600_000
export const DISMISSED_KEY = 'am.judgeBadgeDismissed'
const DISMISSED_MAX = 200

/** 該 bot 近 24 小時超過門檻、最新的一筆；沒有、或已關掉＝`null`。撞限第二意見沒有門檻，不產生徽章。 */
export function judgeBadgeFor(rows: readonly JudgeShadowRow[], botId: string, now: number, dismissed: ReadonlySet<string>): JudgeShadowRow | null {
  let best: JudgeShadowRow | null = null
  let bestT = Number.NEGATIVE_INFINITY
  for (const r of rows) {
    if (r.bot_id !== botId || dismissed.has(r.id) || overThreshold(r) !== true) continue
    const t = Date.parse(r.at)
    if (Number.isNaN(t) || now - t > DAY_MS) continue
    if (t > bestT) {
      best = r
      bestT = t
    }
  }
  return best
}

export function loadDismissed(): Set<string> {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(DISMISSED_KEY) ?? '[]')
    return new Set(Array.isArray(v) ? v.filter((s): s is string => typeof s === 'string').slice(-DISMISSED_MAX) : [])
  } catch {
    return new Set()
  }
}

/** 記下這一則已關掉；回新的集合（只留最近 200 則）。寫不進去（私密視窗、site data 被擋）就只在記憶體裡生效。 */
export function rememberDismissed(id: string, current: ReadonlySet<string>): Set<string> {
  const next = new Set(current)
  next.add(id)
  const kept = [...next].slice(-DISMISSED_MAX)
  try {
    localStorage.setItem(DISMISSED_KEY, JSON.stringify(kept))
  } catch {
    // 寫不進去不影響功能：下次重開可能又會看到同一則，這是可接受的。
  }
  return new Set(kept)
}

/**
 * 浮層的位置（#1381）：貼著徽章鈕左緣、往下開；右邊放不下往左推，離視窗邊留 8px；
 * 下面放不下、上面放得下才往上翻；上下都放不下就維持往下。浮層是 fixed，座標是視窗座標。
 */
export function popPosition(
  btn: { left: number; top: number; bottom: number },
  pop: { width: number; height: number },
  vw: number,
  vh: number,
): { left: number; top: number } {
  const edge = 8
  const left = Math.max(edge, Math.min(btn.left, vw - edge - pop.width))
  const below = btn.bottom + 4
  const above = btn.top - 4 - pop.height
  const top = below + pop.height > vh - edge && above >= edge ? above : below
  return { left, top }
}
