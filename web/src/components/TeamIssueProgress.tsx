import { useRef } from 'react'
import type { Team } from '../api/types'
import { fmtDur, teamProgressOf, useLiveClock } from './teamProgress'

/**
 * 側欄 team 卡片標題底下那一行：**走到第幾個** 與 **已耗時**。
 *
 * 為什麼要有它：卡片上原本只有標題、一顆 phase 燈，收合時多一句「已收合 · 3 位」——
 * 「這隊跑到哪了、跑多久了」兩件事全都看不出來，只能點進去看主面板。20 個 issue 的隊伍
 * 在側欄擺一整天，這兩個數字就是你每次掃過去唯一想知道的東西。
 *
 * 為什麼在標題正下方而不是塞進 tooltip 或收合列：tooltip 要停住游標才看得到，等於沒有；
 * 收合列只在收起來時存在。這一行不隨收合消失，字級也拉到跟標題同級（計數 15px 等寬）。
 * 跑動中用 accent 色、結束轉灰——不用讀字就知道哪一隊還活著。
 */
export function TeamIssueProgress({ team }: { team: Team }) {
  const ref = useRef<HTMLDivElement>(null)
  const p = teamProgressOf(team)
  const running = p.startedAt !== null && p.endedAt === null
  // 只有跑動中才需要每秒重繪；`useLiveClock` 另外會在分頁隱藏／卡片捲出畫面時把 interval 收掉。
  const now = useLiveClock(running, ref)
  const elapsed = p.startedAt === null ? null : (p.endedAt ?? now) - p.startedAt
  if (p.total === 0 && elapsed === null) return null

  const unit = p.kind === 'issues' ? 'issue' : '步驟'
  return (
    <div ref={ref} className={`team-issue-progress${running ? ' running' : ''}`}>
      {p.total > 0 ? (
        <span className="tip-count" title={`已完成 ${p.at} / ${p.total} 個${unit}`}>
          {p.at}
          <span className="tip-total">/{p.total}</span>
        </span>
      ) : null}
      {elapsed !== null ? (
        <span className="tip-elapsed" title={running ? '這個 issue 開跑到現在' : '這個 issue 的總耗時'}>
          {running ? '已 ' : '共 '}
          {fmtDur(elapsed)}
        </span>
      ) : null}
    </div>
  )
}
