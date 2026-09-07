import { useRef } from 'react'
import type { Team } from '../api/types'
import { fmtDur, issueQueueAt, useLiveClock } from './teamProgress'

/**
 * 一隊在跑 20 個 issue 時最想知道的兩件事：走到第幾個、跑了多久。原本只有折疊列上一行
 * 灰字（`20 個 · 已交付 0 · 待處理 19`），要自己減才知道進度，時間更是完全沒有。
 * 這裡把它做成一條進度條 + `1/20` + 計時，常駐在佇列上方（不隨折疊收起來）。
 */
function since(at: string | null, now: number): number | null {
  if (!at) return null
  const t = Date.parse(at)
  return Number.isNaN(t) ? null : now - t
}

export function TeamQueueProgress({ team }: { team: Team }) {
  const ref = useRef<HTMLDivElement>(null)
  const running = team.ended_at === null
  const now = useLiveClock(running, ref)
  const sum = team.issues_summary
  const current = team.issues.find((i) => i.id === team.current_issue_id) ?? null
  // 「第幾個」跟側欄卡片共用同一條算式，兩邊的數字才不會各講各的。
  const at = issueQueueAt(team)
  const donePct = sum.total > 0 ? (sum.done / sum.total) * 100 : 0
  const failPct = sum.total > 0 ? (sum.failed / sum.total) * 100 : 0
  const teamMs = since(team.started_at, running ? now : Date.parse(team.ended_at ?? '') || now)
  const issueMs = current && current.state === 'working' ? since(current.started_at, now) : null

  return (
    <div ref={ref} className="team-progress" title={`已交付 ${sum.done} / 失敗 ${sum.failed} / 待處理 ${sum.queued}，共 ${sum.total} 個 issue`}>
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
