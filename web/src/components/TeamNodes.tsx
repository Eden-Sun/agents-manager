import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot, Team } from '../api/types'
import { TEAM_PHASE_LABEL, TEAM_ROLE_LABEL, TEAM_TERMINAL_PHASES, teamPauseLabel, teamPhaseTone } from '../api/types'
import { botLamp, teamMemberBots, teamShortName, teamsOfProject, useStore } from '../store/store'
import { IdentityBadge } from './IdentitiesPanel'
import { TrashIcon } from './Icons'
import { KindTag } from './KindTag'
import { ModelTag } from './ModelTag'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TeamDeleteDialog } from './TeamDeleteDialog'
import { TeamIssueProgress } from './TeamIssueProgress'

/**
 * SPEC-team §11.4 — sidebar 裡 Project 底下的 Team 節點。
 *
 * 成員 bot 縮排列在節點下，**不與一般 bot 混排**（`Sidebar` 那邊會把 `bot.team` 非 null 的
 * 濾掉）。終態的節點轉灰並提供「清理」。舊 daemon 沒有 `teams` → 這個元件整個不畫。
 */

function MemberRow({ bot }: { bot: Bot }) {
  const lamp = useStore((s) => botLamp(s, bot.id))
  const selected = useStore((s) => s.selectedBotId === bot.id && s.selectedTeamId === null)
  const selectBot = useStore((s) => s.selectBot)
  const role = bot.team?.role ?? 'worker'
  return (
    <div
      className={`bot-row team-member-row${selected ? ' selected' : ''}`}
      role="option"
      aria-selected={selected}
      tabIndex={0}
      title={`${bot.name}（${TEAM_ROLE_LABEL[role]}）：${LAMP_LABEL[lamp]}\n身分 ${bot.identity ?? '預設'}\ncwd ${bot.cwd ?? '（專案根目錄）'}`}
      onClick={() => selectBot(bot.id)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          selectBot(bot.id)
        }
      }}
    >
      <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}`} />
      <span className="bot-main">
        <span className="bot-ident">
          <KindTag kind={bot.kind} className="bot-kind" />
          <span className="bot-name">
            <span className={`team-role ${role}`}>{TEAM_ROLE_LABEL[role]}</span>
            {teamShortName(bot.name)}
          </span>
        </span>
        <span className="bot-sub">
          {/* 和一般 bot 列同一條規則：同一個 CLI 的兩個帳號要分得出來（SPEC §16）。 */}
          <IdentityBadge name={bot.identity} showDefault kind={bot.kind} />
          {/* 和一般 bot 列同一顆籤：模型＋強度。 */}
          <ModelTag botId={bot.id} />
        </span>
      </span>
    </div>
  )
}

function TeamNode({ team }: { team: Team }) {
  const selected = useStore((s) => s.selectedTeamId === team.id)
  const unread = useStore((s) => s.teamUnread[team.id] ?? 0)
  const members = useStore(useShallow((s) => teamMemberBots(s, team.id)))
  const selectTeam = useStore((s) => s.selectTeam)
  const controlTeam = useStore((s) => s.controlTeam)
  const busy = useStore((s) => Boolean(s.busy[`team:${team.id}:cleanup`]))
  const deleting = useStore((s) => Boolean(s.busy[`team:${team.id}:delete`]))
  const [open, setOpen] = useState(true)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const terminal = TEAM_TERMINAL_PHASES.includes(team.phase)
  const tone = teamPhaseTone(team.phase)
  const title = `${team.issue_title || `issue #${team.issue_number}`}`

  return (
    <div className={`team-node${terminal ? ' terminal' : ''}${open ? '' : ' collapsed'}`}>
      <div className={`team-node-head${selected ? ' selected' : ''}`}>
        <button
          type="button"
          className="icon-btn team-node-chev"
          aria-expanded={open}
          aria-label={open ? '收合成員' : '展開成員'}
          title={open ? `收合 ${members.length} 位成員` : `展開 ${members.length} 位成員`}
          onClick={(e) => {
            e.stopPropagation()
            setOpen((v) => !v)
          }}
        >
          <span className="chev">{open ? '▼' : '▶'}</span>
        </button>
        <button
          type="button"
          className="team-node-btn"
          aria-pressed={selected}
          title={`開啟 Team：#${team.issue_number} ${title}\nphase ${TEAM_PHASE_LABEL[team.phase]}${team.pause_reason ? `（${teamPauseLabel(team.pause_reason)}）` : ''}\n分支 ${team.branch}`}
          onClick={() => selectTeam(team.id)}
        >
          <span className="team-icon" aria-hidden="true">
            ⚙
          </span>
          <span className="team-node-label">
            {team.repo ? <span className="team-node-repo mono">{team.repo}</span> : null}#{team.issue_number} {title}
          </span>
          <span className={`team-phase-dot ${tone}`} aria-hidden="true" />
          {!open ? (
            <span className="team-collapsed-label">
              已收合 · {members.length} 位
            </span>
          ) : null}
          {unread > 0 ? (
            <span className="unread-badge" title={`${unread} 則未讀的成員回覆`}>
              {unread > 99 ? '99+' : unread}
            </span>
          ) : null}
        </button>
        {/* 清理與刪除都是不可逆的，卻本來常駐在側欄每一個 team 上（刪除還是紅的）。
            收進一個只在 hover / 鍵盤 focus 時浮出來的殼——選取中的那個 team 也不常駐，
            它的清理／刪除在主面板標題列本來就有。 */}
        <span className="team-node-actions">
        {terminal ? (
          <button
            type="button"
            className="icon-btn icon-tip"
            disabled={busy}
            aria-label={`清理 Team #${team.issue_number}`}
            data-tip={`清理 · #${team.issue_number}`}
            onClick={(e) => {
              e.stopPropagation()
              void controlTeam(team.id, 'cleanup')
            }}
          >
            ✕
          </button>
        ) : null}
        <button
          type="button"
          className="icon-btn icon-tip danger"
          disabled={deleting}
          aria-label={`刪除 Team #${team.issue_number}`}
          data-tip={`刪除 · #${team.issue_number}`}
          onClick={(e) => {
            e.stopPropagation()
            setConfirmDelete(true)
          }}
        >
          <TrashIcon />
        </button>
        </span>
      </div>
      {/* 進度與耗時貼在標題正下方，不隨成員收合消失：這兩個數字是掃過側欄時唯一想知道的。 */}
      <TeamIssueProgress team={team} />
      {open ? (
        <div className="team-node-members" role="group" aria-label={`${members.length} 位成員`}>
          {members.map((b, i) => (
            <div key={b.id} className={`team-member${i === members.length - 1 ? ' last' : ''}`}>
              <MemberRow bot={b} />
            </div>
          ))}
        </div>
      ) : null}
      {confirmDelete ? <TeamDeleteDialog teamId={team.id} onClose={() => setConfirmDelete(false)} /> : null}
    </div>
  )
}


export function TeamNodes({ projectId }: { projectId: string }) {
  const teams = useStore(useShallow((s) => teamsOfProject(s, projectId)))
  if (teams.length === 0) return null
  return (
    <div className="team-nodes">
      {teams.map((t) => (
        <TeamNode key={t.id} team={t} />
      ))}
    </div>
  )
}
