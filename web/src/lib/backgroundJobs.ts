import type { Run } from '../api/types'
import { t } from './i18n'

export function backgroundJobs(run: Run | null | undefined): number {
  if (!run || run.state !== 'running' || run.agent_status !== 'idle') return 0
  return Math.max(0, run.background_jobs ?? 0)
}

export function backgroundLabel(n: number): string {
  return t('背景執行中（{n}）', { n })
}

export function backgroundShortLabel(n: number): string {
  return t('背景 {n}', { n })
}

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

export function backgroundStuck(run: Run | null | undefined): boolean {
  return backgroundJobs(run) > 0 && run?.background_stuck === true
}

export function backgroundStuckLabel(n: number): string {
  return t('背景工作可能卡住（{n}）', { n })
}

export function backgroundDetail(kind: string, n: number): string {
  if (kind === 'codex') return t('回合已經結束，背景還有 {n} 個終端在跑。跑完它會自己接著回報；現在也可以照常送訊息。', { n })
  if (kind === 'agy') return t('回合已經結束，背景還有 {n} 個工作在跑。跑完它會自己接著回報；現在也可以照常送訊息。', { n })
  return t('回合已經結束，背景還有 {n} 個 shell 在跑。跑完它會自己接著回報；現在也可以照常送訊息。', { n })
}

export function backgroundTaskLines(run: Run | null | undefined): string[] {
  const tasks = run?.background_tasks ?? []
  const line = (task: (typeof tasks)[number]) => t('{type}：{description}', { type: task.type, description: task.description || task.command || task.id })
  if (tasks.length <= 3) return tasks.map(line)
  return [...tasks.slice(0, 3).map(line), t('另有 {n} 個', { n: tasks.length - 3 })]
}

export function cronLabel(run: Run | null | undefined): string | null {
  const n = run?.session_crons?.length ?? 0
  return n > 0 ? t('另有 {n} 個排程會叫醒它', { n }) : null
}
