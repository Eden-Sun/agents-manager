/**
 * Jev 帳本（`GET /api/judge/shadow`，SPEC §4.3c）的統計：純函式，給環境設定面板用。
 * 超過門檻的定義照 SPEC：stuck ≥0.7、same_work ≥0.5、report_evidence 的 claims_verified <0.8 或 asks_parent_action ≥0.8。
 * limit_hit 在 SPEC 裡沒有門檻（只記錄、不推提示），所以超過門檻那欄是 `null`＝不適用。
 */
import type { JudgeShadowRow } from '../api/judge'

export type JudgeScene = 'limit_hit' | 'stuck_queued' | 'same_work' | 'report_evidence'

/** 四個場景：名稱與「什麼時候問」（照 SPEC §4.3c 的觸發條件，給沒有紀錄時的說明用）。 */
export const JUDGE_SCENES: readonly { id: JudgeScene; name: string; when: string }[] = [
  {
    id: 'limit_hit',
    name: '撞限第二意見',
    when: 'codex／grok 撞到用量上限，畫面比對判定是新的撞限時，問一題：命中的那一行是介面畫的，還是 bot 印出來的內容。',
  },
  {
    id: 'stuck_queued',
    name: '卡在不認識的畫面',
    when: '有 prompt 排在 queued 超過 180 秒、bot 的 run 是 running／idle、輸入列空著時，問畫面底部是不是有等人選的框。',
  },
  {
    id: 'same_work',
    name: '撞題提示',
    when: '派工或開票時，同專案在跑的工作裡有沒有同一件事，問一次。只提示，不擋派工、不改認領。',
  },
  {
    id: 'report_evidence',
    name: '完成回報的證據',
    when: '交辦從在途收成，且回報是 hook 的完成回覆（不含從終端機刮下來的）時，問回報有沒有宣稱已驗證，以及要不要收件者動手。',
  },
]

export const JUDGE_WINDOWS = [
  { id: 'day', label: '近 24 小時', ms: 24 * 3600_000 },
  { id: 'week', label: '近 7 天', ms: 7 * 24 * 3600_000 },
  { id: 'all', label: '全部', ms: Infinity },
] as const

export type JudgeWindow = (typeof JUDGE_WINDOWS)[number]['id']

/**
 * 撞題紀錄被 daemon 標成過時（答案回來時交辦已不會開始，`matched_line` JSON 的 `stale`＝那時的狀態）；不是就回 null。
 * daemon 刻意不推提示，所以網頁的門檻與徽章也不算它。
 */
export function sameWorkStale(row: JudgeShadowRow): string | null {
  if (row.regex_verdict !== 'same_work' || !row.matched_line) return null
  try {
    const v: unknown = JSON.parse(row.matched_line)
    const s = typeof v === 'object' && v !== null ? (v as Record<string, unknown>).stale : undefined
    return typeof s === 'string' && s !== '' ? s : null
  } catch {
    return null
  }
}

/** 這一列是否超過門檻。`null`＝這個場景沒有門檻。沒有證據（欄位是 null）的一律當沒超過。 */
export function overThreshold(row: JudgeShadowRow): boolean | null {
  switch (row.regex_verdict) {
    case 'stuck_queued':
      return row.jev_is_live_ui !== null && row.jev_is_live_ui >= 0.7
    case 'same_work':
      return sameWorkStale(row) === null && row.jev_same_work !== null && row.jev_same_work >= 0.5
    case 'report_evidence':
      return (row.claims_verified !== null && row.claims_verified < 0.8) || (row.asks_parent_action !== null && row.asks_parent_action >= 0.8)
    default:
      return null
  }
}

export interface SceneWindowCount {
  total: number
  /** 超過門檻的筆數；場景沒有門檻時是 `null`。 */
  over: number | null
  errors: number
}

export interface SceneStat {
  scene: JudgeScene
  windows: Record<JudgeWindow, SceneWindowCount>
  /** 最後一次（`at` 最新的那列）的時間原字串；沒有紀錄是 `null`。 */
  last: string | null
}

const isError = (row: JudgeShadowRow) => row.error !== null && row.error !== ''

function inWindow(at: string, ms: number, now: number): boolean {
  const t = Date.parse(at)
  return Number.isNaN(t) ? false : now - t <= ms
}

/** 每個場景各一列統計（順序照 `JUDGE_SCENES`）。`now` 可注入，測試才決定性。 */
export function sceneStats(rows: readonly JudgeShadowRow[], now: number = Date.now()): SceneStat[] {
  return JUDGE_SCENES.map(({ id }) => {
    const mine = rows.filter((r) => r.regex_verdict === id)
    const windows = {} as Record<JudgeWindow, SceneWindowCount>
    for (const w of JUDGE_WINDOWS) {
      const picked = w.id === 'all' ? mine : mine.filter((r) => inWindow(r.at, w.ms, now))
      const over = id === 'limit_hit' ? null : picked.filter((r) => overThreshold(r) === true).length
      windows[w.id] = { total: picked.length, over, errors: picked.filter(isError).length }
    }
    let last: string | null = null
    let lastT = Number.NaN
    for (const r of mine) {
      const t = Date.parse(r.at)
      if (!Number.isNaN(t) && (Number.isNaN(lastT) || t > lastT)) {
        lastT = t
        last = r.at
      }
    }
    return { scene: id, windows, last }
  })
}

/** 最近的幾列，新的在前（時間解不開的排最後）。 */
export function recentRows(rows: readonly JudgeShadowRow[], limit = 20): JudgeShadowRow[] {
  const key = (r: JudgeShadowRow) => {
    const t = Date.parse(r.at)
    return Number.isNaN(t) ? Number.NEGATIVE_INFINITY : t
  }
  return [...rows].sort((a, b) => key(b) - key(a)).slice(0, limit)
}

const fmt = (n: number | null) => (n === null ? '—' : n.toFixed(2))

/** 表格裡「機率」那一欄：照場景取對應的欄位。 */
export function probabilityText(row: JudgeShadowRow): string {
  switch (row.regex_verdict) {
    case 'stuck_queued':
    case 'limit_hit':
      return fmt(row.jev_is_live_ui)
    case 'same_work':
      return fmt(row.jev_same_work)
    case 'report_evidence':
      return `驗 ${fmt(row.claims_verified)}／要 ${fmt(row.asks_parent_action)}`
    default:
      return '—'
  }
}

/** bot 名（store 找得到用名字）；找不到顯示 id 前 6 碼；沒有 bot 是 `—`。 */
export function botLabel(id: string | null, bots: readonly { id: string; name: string }[]): string {
  if (id === null) return '—'
  // 開票那條的候選沒有 bot，bot_id 是 issue:<號碼>（release_triage 開票後的撞題檢查）；取前 6 碼會只剩 issue:。
  const issue = /^issue:(\d+)$/.exec(id)
  if (issue) return `#${issue[1]}（開票）`
  return bots.find((b) => b.id === id)?.name ?? id.slice(0, 6)
}
