import { useRef } from 'react'
import type { Team } from '../api/types'
import type { TeamCount } from './teamProgress'
import { fmtDur, teamProgressOf, useLiveClock } from './teamProgress'

/**
 * 側欄 team 卡片標題底下那一行：**走到第幾個** 與 **已耗時**。
 *
 * 為什麼要有它：卡片上原本只有標題、一顆 phase 燈，收合時多一句「已收合 · 3 位」——
 * 「這隊跑到哪了、跑多久了」兩件事全都看不出來，只能點進去看主面板。20 個 issue 的隊伍
 * 在側欄擺一整天，這兩個數字就是你每次掃過去唯一想知道的東西。
 *
 * 為什麼在標題正下方而不是塞進 tooltip 或收合列：tooltip 要停住游標才看得到，等於沒有；
 * 收合列只在收起來時存在。這一行不隨收合消失，字級也拉到跟標題同級（計數 14px 等寬）。
 * 跑動中用 accent 色、結束轉灰——不用讀字就知道哪一隊還活著。
 *
 * 計數帶單位（`issue 2/20 · task 4/5`）：原本只有一個沒頭沒尾的 `2/20`，同一隊已經合併
 * 4 個 task 卻寫著 2，使用者只能猜那是什麼、並且以為卡住了（issue #53）。
 */
function Count({ unit, count, title }: { unit: string; count: TeamCount; title: string }) {
  return (
    <span className="tip-count" title={title}>
      <span className="tip-unit">{unit}</span>
      {count.at}
      <span className="tip-total">/{count.total}</span>
    </span>
  )
}

export function TeamIssueProgress({ team }: { team: Team }) {
  const ref = useRef<HTMLDivElement>(null)
  const p = teamProgressOf(team)
  const running = p.startedAt !== null && p.frozenMs === null
  // 只有跑動中才需要每秒重繪；`useLiveClock` 另外會在分頁隱藏／卡片捲出畫面時把 interval 收掉。
  const now = useLiveClock(running, ref)
  const elapsed = p.frozenMs ?? (p.startedAt === null ? null : now - p.startedAt)
  const queued = p.issues.total > 1
  if (p.tasks.total === 0 && !queued && elapsed === null) return null

  return (
    <div ref={ref} className={`team-issue-progress${running ? ' running' : ''}`}>
      {queued && p.workingIssues > 1 ? (
        // §4.5 無限模式：好幾個 issue 同時在跑，「第幾個」講不通——報「進行中 N」。
        <span className="tip-count" title={`同時進行 ${p.workingIssues} 個 issue，佇列共 ${p.issues.total} 個`}>
          <span className="tip-unit">issue</span>
          進行中 {p.workingIssues}
          <span className="tip-total">/{p.issues.total}</span>
        </span>
      ) : queued ? (
        <Count
          unit="issue"
          count={p.issues}
          title={`正在做第 ${p.issues.at} 個 issue，佇列共 ${p.issues.total} 個`}
        />
      ) : null}
      {p.tasks.total > 0 ? (
        <Count
          unit="task"
          count={p.tasks}
          title={`這個 issue 已結案 ${p.tasks.at} / ${p.tasks.total} 個 task（合併／略過／失敗）`}
        />
      ) : null}
      {elapsed !== null ? (
        <span
          className="tip-elapsed"
          title={running ? '這個 issue 開跑到現在' : '這個 issue 停下來為止的耗時（暫停中不再累加）'}
        >
          {p.endedAt !== null ? '共 ' : '已 '}
          {fmtDur(elapsed)}
        </span>
      ) : null}
    </div>
  )
}
