import type { Run } from '../api/types'

/**
 * #714：回合結束了（agent idle、可以收訊息），但畫面底部還標著背景工作在跑（claude 的 shell、codex 的背景終端）。
 * 這時候不能只顯示「閒置」：使用者會以為它停了。回合中（working）或不在跑就不另外標——那時燈號已經說清楚了。
 */
export function backgroundJobs(run: Run | null | undefined): number {
  if (!run || run.state !== 'running' || run.agent_status !== 'idle') return 0
  return Math.max(0, run.background_jobs ?? 0)
}

export function backgroundLabel(n: number): string {
  return `背景執行中（${n}）`
}

/** 側欄那格原本只放「閒置」兩個字，完整字面放不下（會被截成「背景執」，連「（N）」都放不下）。 */
export function backgroundShortLabel(n: number): string {
  return `背景 ${n}`
}

export function backgroundDetail(kind: string, n: number): string {
  const what = kind === 'codex' ? `${n} 個終端` : `${n} 個 shell `
  return `回合已經結束，背景還有 ${what}在跑。跑完它會自己接著回報；現在也可以照常送訊息。`
}

/** hook 報的背景工作明細（最多三行，其餘折成「另有 N 個」）；沒有明細（畫面判斷的數字）是空陣列。 */
export function backgroundTaskLines(run: Run | null | undefined): string[] {
  const tasks = run?.background_tasks ?? []
  const line = (t: (typeof tasks)[number]) => `${t.type}：${t.description || t.command || t.id}`
  if (tasks.length <= 3) return tasks.map(line)
  return [...tasks.slice(0, 3).map(line), `另有 ${tasks.length - 3} 個`]
}

/** session 排程（/loop、ScheduleWakeup…）：不算背景工作，只提醒「之後會被叫醒」。 */
export function cronLabel(run: Run | null | undefined): string | null {
  const n = run?.session_crons?.length ?? 0
  return n > 0 ? `另有 ${n} 個排程會叫醒它` : null
}
