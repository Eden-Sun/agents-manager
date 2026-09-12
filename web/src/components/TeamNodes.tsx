import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot, Team } from '../api/types'
import { TEAM_PHASE_LABEL, TEAM_ROLE_LABEL, TEAM_TERMINAL_PHASES, roleKeyOfMember, teamPauseLabel, teamPhaseTone } from '../api/types'
import { botLamp, teamMemberBots, teamsOfProject, useStore } from '../store/store'
import { teamDisplayName } from '../api/types'
import { IdentityBadge } from './IdentitiesPanel'
import { TrashIcon } from './Icons'
import { KindTag } from './KindTag'
import { ModelTag } from './ModelTag'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TeamCleanupDialog } from './TeamCleanupDialog'
import { TeamDeleteDialog } from './TeamDeleteDialog'
import { TeamIssueProgress } from './TeamIssueProgress'
import { teamTitle } from './TeamNameField'
import { teamPauseAction, teamPauseDetailLines } from './teamPanelLogic'

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
  const openTeamRole = useStore((s) => s.openTeamRole)
  const role = bot.team?.role ?? 'worker'
  const teamId = bot.team?.team_id ?? null
  // 無限併行時每個 issue 各有一個 dev-1：同名就帶著 i<seq>-。
  const siblings = useStore(useShallow((s) => teamMemberBots(s, teamId).map((b) => b.name)))
  return (
    <div
      className={`bot-row team-member-row${selected ? ' selected' : ''}`}
      // 跟側欄的 bot 列同一套：清單項目（裡面有齒輪鍵，不能是 option），選取中用 aria-current。
      role="listitem"
      aria-current={selected ? 'true' : undefined}
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
            {teamDisplayName(bot.name, siblings)}
          </span>
        </span>
        <span className="bot-sub">
          {/* 和一般 bot 列同一條規則：同一個 CLI 的兩個帳號要分得出來（SPEC §16）。 */}
          <IdentityBadge name={bot.identity} showDefault kind={bot.kind} />
          {/* 和一般 bot 列同一顆籤：模型＋強度。 */}
          <ModelTag botId={bot.id} />
        </span>
      </span>
      {/* 齒輪開的是 **Team 的角色設定**，不是 bot 設定：team 成員是從 `roles_json` 建出來的，
          只改這一列的 bot 不會動到那份 spec，下一批又會跑回舊設定（SPEC-team §10.5）。 */}
      {teamId ? (
        <button
          type="button"
          className="icon-btn icon-tip team-member-gear"
          aria-label={`${TEAM_ROLE_LABEL[role]}的 Team 設定`}
          data-tip={`${TEAM_ROLE_LABEL[role]} 設定`}
          onClick={(e) => {
            e.stopPropagation()
            openTeamRole(teamId, roleKeyOfMember(role))
          }}
        >
          ⚙
        </button>
      ) : null}
    </div>
  )
}

/**
 * 暫停中的隊伍在側欄那一行：**寫出原因**，能推得動的再給一顆按鈕。
 *
 * 原本 `paused` 只反映在標題列的一顆褐色點與 tooltip 上（tooltip 要停住游標才看得到，
 * 等於沒有）。#53 因為 `budget_time` 停了兩個多小時，側欄看起來跟「還在跑」沒兩樣，
 * 使用者以為卡死——推得動它的兩顆按鈕全在 TeamPanel 裡，要先點進去才知道。
 *
 * 按鈕的口徑與 TeamPanel 標題列同一條（`teamPauseAction`）：預算類加碼再繼續，
 * 其餘可推的直接繼續，要回話／放行／救成員的不給按鈕（那些在面板裡才處理得掉）。
 */
function TeamPausedRow({ team }: { team: Team }) {
  const controlTeam = useStore((s) => s.controlTeam)
  const patchTeam = useStore((s) => s.patchTeam)
  const busy = useStore((s) => Boolean(s.busy[`team:${team.id}:resume`] || s.busy[`team:${team.id}:patch`]))
  const reason = team.pause_reason
  const action = teamPauseAction(reason)
  const label = teamPauseLabel(reason)

  return (
    <div className="team-paused-row">
      {/* 側欄這一列窄，寫不下整句「rev（cc2）5h 額度剩 4%」；名字進 tooltip，全文在面板橫幅。 */}
      <span className="team-paused-why" title={[reason, teamPauseDetailLines(team)].filter(Boolean).join('\n') || undefined}>
        已暫停{label ? ` · ${label}` : ''}
      </span>
      {action !== null ? (
        <button
          type="button"
          className="mini-btn team-paused-go"
          disabled={busy}
          title={
            action === 'bump'
              ? '把轉送上限與時間上限各加一倍，然後從暫停的地方繼續'
              : action === 'force'
                ? '把這個 team 的額度門檻設成 100%（之後不再因額度暫停），然後從暫停的地方繼續'
                : '從暫停的地方繼續（會重送待送的轉送）'
          }
          onClick={(e) => {
            // 側欄的一列同時是「選取這個 team」的按鈕，按這顆不該順便切畫面。
            e.stopPropagation()
            void (async () => {
              if (action === 'bump') {
                const ok = await patchTeam(team.id, {
                  budget: {
                    max_relays: team.budget.max_relays * 2,
                    max_wall_clock_min: team.budget.max_wall_clock_min * 2,
                  },
                })
                if (!ok) return
              } else if (action === 'force') {
                const ok = await patchTeam(team.id, { budget: { quota_stop_pct: 100 } })
                if (!ok) return
              }
              await controlTeam(team.id, 'resume')
            })()
          }}
        >
          {action === 'bump' ? '加碼並繼續' : action === 'force' ? '無視額度繼續' : '繼續'}
        </button>
      ) : null}
    </div>
  )
}

function TeamNode({ team }: { team: Team }) {
  const selected = useStore((s) => s.selectedTeamId === team.id)
  const unread = useStore((s) => s.teamUnread[team.id] ?? 0)
  const members = useStore(useShallow((s) => teamMemberBots(s, team.id)))
  const selectTeam = useStore((s) => s.selectTeam)
  const busy = useStore((s) => Boolean(s.busy[`team:${team.id}:cleanup`]))
  const deleting = useStore((s) => Boolean(s.busy[`team:${team.id}:delete`]))
  const [open, setOpen] = useState(true)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmCleanup, setConfirmCleanup] = useState(false)
  const terminal = TEAM_TERMINAL_PHASES.includes(team.phase)
  const tone = teamPhaseTone(team.phase)
  const title = teamTitle(team)

  return (
    <div className={`team-node${terminal ? ' terminal' : ''}${team.phase === 'paused' ? ' paused' : ''}${open ? '' : ' collapsed'}`}>
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
          // UI-DECISIONS #11：「開著的是這個」統一用 aria-current（bot 列、專案標題鍵都是）。
          aria-current={selected ? 'true' : undefined}
          title={`開啟 Team：#${team.issue_number} ${title}\nphase ${TEAM_PHASE_LABEL[team.phase]}${team.pause_reason ? `（${teamPauseLabel(team.pause_reason)}）` : ''}\n分支 ${team.branch}`}
          onClick={() => selectTeam(team.id)}
        >
          <span className="team-icon" aria-hidden="true">
            ⚙
          </span>
          <span className="team-node-label">
            {team.repo ? <span className="team-node-repo mono">{team.repo}</span> : null}
            {title}
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
              // 清理跟刪除一樣不可逆（成員軟刪、worktree 移除），主面板那顆走確認框，這裡也要。
              setConfirmCleanup(true)
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
      {team.phase === 'paused' ? <TeamPausedRow team={team} /> : null}
      {open ? (
        <div className="team-node-members" role="list" aria-label={`${members.length} 位成員`}>
          {members.map((b, i) => (
            <div key={b.id} className={`team-member${i === members.length - 1 ? ' last' : ''}`}>
              <MemberRow bot={b} />
            </div>
          ))}
        </div>
      ) : null}
      {confirmDelete ? <TeamDeleteDialog teamId={team.id} onClose={() => setConfirmDelete(false)} /> : null}
      {confirmCleanup ? <TeamCleanupDialog teamId={team.id} onClose={() => setConfirmCleanup(false)} /> : null}
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
