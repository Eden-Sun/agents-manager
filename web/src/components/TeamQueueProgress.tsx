import { useEffect, useState } from 'react'
import type { Team } from '../api/types'

/**
 * 一隊在跑 20 個 issue 時最想知道的兩件事：走到第幾個、跑了多久。原本只有折疊列上一行
 * 灰字（`20 個 · 已交付 0 · 待處理 19`），要自己減才知道進度，時間更是完全沒有。
 * 這裡把它做成一條進度條 + `1/20` + 計時，常駐在佇列上方（不隨折疊收起來）。
 */
function fmtDur(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  if (h > 0) return `${h} 小時 ${m} 分`
  if (m > 0) return `${m} 分 ${sec} 秒`
  return `${sec} 秒`
}

/** 秒級計時：這一整段每秒重算一次，不影響 TeamPanel 其他部分。 */
function useTick(active: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [active])
  return now
}

function since(at: string | null, now: number): number | null {
  if (!at) return null
  const t = Date.parse(at)
  return Number.isNaN(t) ? null : now - t
}

export function TeamQueueProgress({ team }: { team: Team }) {
  const running = team.ended_at === null
  const now = useTick(running)
  const sum = team.issues_summary
  const settled = sum.done + sum.failed
  const current = team.issues.find((i) => i.id === team.current_issue_id) ?? null
  // 進行中的那一個算「第幾個」：交付 1 個、正在做第 2 個 → 2/20。全部結束時就是 total/total。
  const at = Math.min(sum.total, settled + (current && current.state === 'working' ? 1 : 0))
  const donePct = sum.total > 0 ? (sum.done / sum.total) * 100 : 0
  const failPct = sum.total > 0 ? (sum.failed / sum.total) * 100 : 0
  const teamMs = since(team.started_at, running ? now : Date.parse(team.ended_at ?? '') || now)
  const issueMs = current && current.state === 'working' ? since(current.started_at, now) : null

  return (
    <div className="team-progress" title={`已交付 ${sum.done} / 失敗 ${sum.failed} / 待處理 ${sum.queued}，共 ${sum.total} 個 issue`}>
      <span className="team-progress-count">
        {at}
        <span className="team-progress-total">/{sum.total}</span>
      </span>
      <span className="team-progress-bar" aria-hidden="true">
        <span className="team-progress-done" style={{ width: `${donePct}%` }} />
        <span className="team-progress-fail" style={{ width: `${failPct}%` }} />
      </span>
      <span className="team-progress-time">
        {issueMs !== null ? <strong>本 issue 已 {fmtDur(issueMs)}</strong> : null}
        {issueMs !== null && teamMs !== null ? ' · ' : null}
        {teamMs !== null ? `整隊 ${fmtDur(teamMs)}` : null}
      </span>
    </div>
  )
}
