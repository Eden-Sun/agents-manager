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
