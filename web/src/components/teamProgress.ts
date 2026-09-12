import { useEffect, useMemo, useReducer, useState } from 'react'
import type { RefObject } from 'react'
import type { Team, TeamTaskState } from '../api/types'
import { TEAM_TERMINAL_PHASES } from '../api/types.ts'

/**
 * 「這張卡跑到哪了、跑了多久」——側欄 team 卡片與 `TeamQueueProgress` 共用的口徑。
 *
 * daemon 沒有一個現成的「回合數／總回合數」，所以計數是從既有欄位算出來的（口徑寫在
 * docs/SPEC-team.md §2.3）。兩個計數**一起回**，呼叫端自己決定畫哪個：
 * - `issues`：「正在做第幾個 issue」，跟佇列進度條同一條算式；`total <= 1` 就不必畫。
 * - `tasks`：當前這個 issue 已結案的 task 數（`merged` / `skipped` / `failed`）／`total`。
 *   PM 還沒拆 task 時 `total` 是 0，那就只報時間。
 *
 * 原本只回其中一個、而且沒有單位字（`2/20`）——20 個 issue 的隊伍做完 4 個 task 只看到
 * 「2/20」，使用者以為是 task 卡在 2。兩個數字都給，並在畫面上寫出 `issue` / `task`。
 */

/** task 進到這些狀態就不會再動了——「完成的步驟」數的是它們。 */
const SETTLED_TASKS: readonly TeamTaskState[] = ['merged', 'skipped', 'failed']

/** 一組「第幾個 / 共幾個」。`total === 0` 時沒有意義，呼叫端不要顯示。 */
export interface TeamCount {
  at: number
  total: number
}

export interface TeamProgress {
  /** 佇列走到第幾個；`total <= 1`（沒有佇列）時呼叫端不要顯示。 */
  issues: TeamCount
  /**
   * 同時進行中的 issue 數（SPEC-team §4.5 無限模式）。有限模式恆為 0 或 1，呼叫端只在
   * `> 1` 時改口徑：「第幾個 / 共幾個」在多個同時進行時是錯的說法。
   */
  workingIssues: number
  /** 當前這個 issue 的 task 走到第幾個。 */
  tasks: TeamCount
  /** 計時器從哪一刻起算（epoch ms）；null = 這個 issue 還沒開跑。 */
  startedAt: number | null
  /** 跑完凍結在這一刻；null = 還沒結束。 */
  endedAt: number | null
  /**
   * 已經不會再前進的耗時（ms）；null = 還在跑，呼叫端要自己接時鐘。
   *
   * `paused` 也算不動了：暫停的隊伍沒人在燒時間，計時器卻照著 `started_at` 一直跳
   * （#53 停在 119 分卻顯示「已 4 小時 38 分」），看起來像是卡死在跑。終態同理。
   */
  frozenMs: number | null
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
  const settled = sum.done + sum.failed
  // §4.5 無限模式：同時可能有好幾個 working，全部都算「已經開工的」。
  return Math.min(sum.total, settled + workingIssuesOf(team))
}

/** 現在同時有幾個 issue 在跑。有限模式是 0 或 1；無限模式最多 `TEAM_MAX_CONCURRENT_ISSUES`。 */
export function workingIssuesOf(team: Team): number {
  return team.issues.filter((i) => i.state === 'working').length
}

export function teamProgressOf(team: Team): TeamProgress {
  const current = team.issues.find((i) => i.id === team.current_issue_id) ?? null
  const teamEnded = parse(team.ended_at)
  // 計時從「這個 issue」開跑算起（SPEC-team §2.3：預算本來就是每個 issue 算的）。
  // 佇列裡沒有對應那一項的舊 team 就退回整隊的時間。
  // §4.5 無限模式有好幾個 working，`current` 只是第一個；計時取最早開跑的那一個，
  // 「這隊跑多久了」才不會在第一個 issue 交付後突然跳回幾分鐘前。
  const oldest = team.issues
    .filter((i) => i.state === 'working' && i.started_at)
    .map((i) => parse(i.started_at))
    .filter((t): t is number => t !== null)
    .sort((a, b) => a - b)[0]
  const startedAt = oldest ?? parse(current?.started_at) ?? parse(team.started_at)
  // 整隊結束了就算 issue 自己沒寫 ended_at 也要凍結——不然做完的卡片會一直跳秒。
  const endedAt = parse(current?.ended_at) ?? teamEnded

  const tasks = team.tasks_summary
  const settled = SETTLED_TASKS.reduce((n, s) => n + (tasks[s] ?? 0), 0)
  return {
    issues: { at: issueQueueAt(team), total: team.issues_summary.total },
    workingIssues: workingIssuesOf(team),
    tasks: { at: Math.min(tasks.total, settled), total: tasks.total },
    startedAt,
    endedAt,
    frozenMs: frozenMsOf(team, startedAt, endedAt),
  }
}

/**
 * 結束了就用真正的區間；還沒結束但已經停下來（`paused` / 終態沒寫 `ended_at`）就用
 * daemon 自己算的 `usage.elapsed_min`——它跟 `max_wall_clock_min` 是同一個數字，
 * 而且暫停之後 scheduler 不再更新它，正好就是「停住的那一刻」。
 */
function frozenMsOf(team: Team, startedAt: number | null, endedAt: number | null): number | null {
  if (startedAt !== null && endedAt !== null) return Math.max(0, endedAt - startedAt)
  const halted = team.phase === 'paused' || TEAM_TERMINAL_PHASES.includes(team.phase)
  if (!halted || startedAt === null) return null
  return Math.max(0, team.usage?.elapsed_min ?? 0) * 60_000
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
