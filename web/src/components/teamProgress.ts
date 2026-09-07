import { useEffect, useMemo, useReducer, useState } from 'react'
import type { RefObject } from 'react'
import type { Team, TeamTaskState } from '../api/types'

/**
 * 「這張卡跑到哪了、跑了多久」——側欄 team 卡片與 `TeamQueueProgress` 共用的口徑。
 *
 * daemon 沒有一個現成的「回合數／總回合數」，所以計數是從既有欄位算出來的，而且分兩種
 * （見 `TeamProgress.kind`，口徑寫在 docs/SPEC-team.md §2.3）：
 * - 佇列（`issues_summary.total >= 2`）：分子是「正在做第幾個 issue」，跟佇列進度條同一條算式。
 * - 單一 issue：分子是已結案的 task 數（`tasks_summary` 的 merged / skipped / failed），
 *   分母是 `tasks_summary.total`。PM 還沒拆 task 時 total 是 0，那就只報時間、不報計數。
 */

/** task 進到這些狀態就不會再動了——「完成的步驟」數的是它們。 */
const SETTLED_TASKS: readonly TeamTaskState[] = ['merged', 'skipped', 'failed']

export type TeamProgressKind = 'issues' | 'tasks'

export interface TeamProgress {
  kind: TeamProgressKind
  /** 分子。`total === 0` 時沒有意義，呼叫端不要顯示計數。 */
  at: number
  total: number
  /** 計時器從哪一刻起算（epoch ms）；null = 這個 issue 還沒開跑。 */
  startedAt: number | null
  /** 跑完凍結在這一刻；null = 還在跑。 */
  endedAt: number | null
}

function parse(at: string | null | undefined): number | null {
  if (!at) return null
  const t = Date.parse(at)
  return Number.isNaN(t) ? null : t
}

/**
 * 佇列走到第幾個：交付 1 個、正在做第 2 個 → 2。全部結束時等於 total。
 * `TeamQueueProgress` 也用這一條，兩邊的數字才不會各講各的。
 */
export function issueQueueAt(team: Team): number {
  const sum = team.issues_summary
  const current = team.issues.find((i) => i.id === team.current_issue_id) ?? null
  const settled = sum.done + sum.failed
  return Math.min(sum.total, settled + (current && current.state === 'working' ? 1 : 0))
}

export function teamProgressOf(team: Team): TeamProgress {
  const current = team.issues.find((i) => i.id === team.current_issue_id) ?? null
  const teamEnded = parse(team.ended_at)
  // 計時從「這個 issue」開跑算起（SPEC-team §2.3：預算本來就是每個 issue 算的）。
  // 佇列裡沒有對應那一項的舊 team 就退回整隊的時間。
  const startedAt = parse(current?.started_at) ?? parse(team.started_at)
  // 整隊結束了就算 issue 自己沒寫 ended_at 也要凍結——不然做完的卡片會一直跳秒。
  const endedAt = parse(current?.ended_at) ?? teamEnded

  const queue = team.issues_summary
  if (queue.total >= 2) {
    return { kind: 'issues', at: issueQueueAt(team), total: queue.total, startedAt, endedAt }
  }
  const tasks = team.tasks_summary
  const at = SETTLED_TASKS.reduce((n, s) => n + (tasks[s] ?? 0), 0)
  return { kind: 'tasks', at: Math.min(tasks.total, at), total: tasks.total, startedAt, endedAt }
}

/** `已 3 分 12 秒` 的數字部分。超過一小時就不報秒——那個位數已經沒人在看了。 */
export function fmtDur(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  if (h > 0) return `${h} 小時 ${m} 分`
  if (m > 0) return `${m} 分 ${sec} 秒`
  return `${sec} 秒`
}

/**
 * 每秒一次的重繪，但只在真的看得到的時候：跑完了、分頁切到背景、卡片捲出畫面，
 * 都把 interval 收掉，側欄十幾個 team 才不會在沒人看的時候每秒喚醒十幾次。
 *
 * 回傳的是 render 當下的時鐘。停掉再回來時那一次 re-render 本來就會發生（`live` 變了），
 * 所以不必在 effect 裡補一次 setState——值自然就是新的。
 */
export function useLiveClock(active: boolean, ref: RefObject<HTMLElement | null>): number {
  const [tick, bump] = useReducer((n: number) => n + 1, 0)
  const [onScreen, setOnScreen] = useState(true)
  const [visible, setVisible] = useState(() => typeof document === 'undefined' || !document.hidden)

  useEffect(() => {
    const el = ref.current
    if (!el || typeof IntersectionObserver === 'undefined') return
    const io = new IntersectionObserver((entries) => setOnScreen(entries.some((e) => e.isIntersecting)))
    io.observe(el)
    return () => io.disconnect()
  }, [ref])

  useEffect(() => {
    const on = () => setVisible(!document.hidden)
    document.addEventListener('visibilitychange', on)
    return () => document.removeEventListener('visibilitychange', on)
  }, [])

  const live = active && onScreen && visible
  useEffect(() => {
    if (!live) return
    const id = setInterval(bump, 1000)
    return () => clearInterval(id)
  }, [live])

  // eslint-disable-next-line react-hooks/exhaustive-deps
  return useMemo(() => Date.now(), [tick, live])
}
