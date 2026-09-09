import { useEffect, useRef, useState } from 'react'
import type { Team } from '../api/types'
import { useStore } from '../store/store'

/**
 * team 顯示用的名字。使用者取的短名優先，沒取就用 `#編號 issue 標題`——issue 標題常常是
 * 一整句規格（「#48 [arch] 回合完成判定改成單一狀態機：hook > agent_status …」），在手機的
 * 標題列與切換器裡只看得到前八個字，等於認不出是哪一隊。
 */
export function teamTitle(team: Pick<Team, 'label' | 'issue_number' | 'issue_title'>): string {
  const label = team.label?.trim()
  if (label) return label
  return `#${team.issue_number} ${team.issue_title || ''}`.trim()
}

/**
 * 點一下改名（同 `BotNameField` 的做法）。清空送出＝改回 issue 標題。
 * 已經結束的 team 也能改：名字是給人事後找東西用的。
 */
export function TeamNameField({ teamId, className }: { teamId: string; className?: string }) {
  const team = useStore((s) => s.teams[teamId] ?? null)
  const patchTeam = useStore((s) => s.patchTeam)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const ref = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (editing) ref.current?.select()
  }, [editing])

  if (!team) return null
  const shown = teamTitle(team)

  const commit = () => {
    setEditing(false)
    const next = draft.trim().slice(0, 60)
    if (next === (team.label ?? '')) return
    void patchTeam(teamId, { label: next })
  }

  if (editing) {
    return (
      <input
        ref={ref}
        type="text"
        className={`bot-name-input head${className ? ` ${className}` : ''}`}
        value={draft}
        spellCheck={false}
        maxLength={60}
        aria-label="Team 名稱"
        placeholder={`#${team.issue_number} ${team.issue_title}`}
        title="留白＝改回 issue 標題"
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onClick={(e) => e.stopPropagation()}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          e.stopPropagation()
          if (e.key === 'Enter') {
            e.preventDefault()
            commit()
          } else if (e.key === 'Escape') {
            e.preventDefault()
            setEditing(false)
          }
        }}
      />
    )
  }

  return (
    <button
      type="button"
      className={`bot-name-btn team-name-btn${className ? ` ${className}` : ''}`}
      title={`${shown}\n#${team.issue_number} ${team.issue_title}\n點一下改名（留白＝改回 issue 標題）`}
      onClick={() => {
        setDraft(team.label ?? '')
        setEditing(true)
      }}
    >
      <strong>{shown}</strong>
    </button>
  )
}
