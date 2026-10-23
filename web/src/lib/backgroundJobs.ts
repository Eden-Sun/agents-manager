import type { Run } from '../api/types'
import { t } from './i18n'

/**
 * #714：回合結束了（agent idle、可以收訊息），但畫面底部還標著背景工作在跑（claude 的 shell、codex 的背景終端）。
 * 這時候不能只顯示「閒置」：使用者會以為它停了。回合中（working）或不在跑就不另外標——那時燈號已經說清楚了。
 */
export function backgroundJobs(run: Run | null | undefined): number {
  if (!run || run.state !== 'running' || run.agent_status !== 'idle') return 0
  return Math.max(0, run.background_jobs ?? 0)
}

export function backgroundLabel(n: number): string {
  return t('背景執行中（{n}）', { n })
}

/** 側欄那格原本只放「閒置」兩個字，完整字面放不下（會被截成「背景執」，連「（N）」都放不下）。 */
export function backgroundShortLabel(n: number): string {
  return t('背景 {n}', { n })
}

/** #774：背景跑了多久（`background_since` 到 `now`），例如「2 小時 5 分」；沒有開始時間是 null。 */
export function backgroundAge(run: Run | null | undefined, now: number = Date.now()): string | null {
  const since = run?.background_since ? Date.parse(run.background_since) : NaN
  if (Number.isNaN(since)) return null
  const min = Math.max(0, Math.floor((now - since) / 60_000))
  if (min < 1) return t('不到 1 分鐘')
  const h = Math.floor(min / 60)
  return h > 0
    ? min % 60
      ? t('{hours} 小時 {minutes} 分', { hours: h, minutes: min % 60 })
      : t('{n} 小時', { n: h })
    : t('{n} 分鐘', { n: min })
}

/** #774：claude 2.1.288 起終端 session 的背景指令沒有時間上限；daemon 判定標太久（`background_stuck`）時換字。 */
export function backgroundStuck(run: Run | null | undefined): boolean {
  return backgroundJobs(run) > 0 && run?.background_stuck === true
}

export function backgroundStuckLabel(n: number): string {
  return t('背景工作可能卡住（{n}）', { n })
}

export function backgroundDetail(kind: string, n: number): string {
  return kind === 'codex'
    ? t('回合已經結束，背景還有 {n} 個終端在跑。跑完它會自己接著回報；現在也可以照常送訊息。', { n })
    : t('回合已經結束，背景還有 {n} 個 shell 在跑。跑完它會自己接著回報；現在也可以照常送訊息。', { n })
}

/** hook 報的背景工作明細（最多三行，其餘折成「另有 N 個」）；沒有明細（畫面判斷的數字）是空陣列。 */
export function backgroundTaskLines(run: Run | null | undefined): string[] {
  const tasks = run?.background_tasks ?? []
  const line = (task: (typeof tasks)[number]) => t('{type}：{description}', { type: task.type, description: task.description || task.command || task.id })
  if (tasks.length <= 3) return tasks.map(line)
  return [...tasks.slice(0, 3).map(line), t('另有 {n} 個', { n: tasks.length - 3 })]
}

/** session 排程（/loop、ScheduleWakeup…）：不算背景工作，只提醒「之後會被叫醒」。 */
export function cronLabel(run: Run | null | undefined): string | null {
  const n = run?.session_crons?.length ?? 0
  return n > 0 ? t('另有 {n} 個排程會叫醒它', { n }) : null
}
