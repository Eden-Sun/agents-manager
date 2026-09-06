import { useState } from 'react'
import { createPortal } from 'react-dom'
import type { TeamBranchDisposal } from '../api/types'
import { TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * SPEC-team §6.5a「刪除 Team」的確認流程。TeamPanel 的標題列與 Sidebar 的 team 節點共用，
 * 免得兩邊的文案漂移（刪除的後果只能寫一次）。
 *
 * 兩種強度：
 * - `branches=keep`（**預設**）：一般確認。列出會發生的事，並明說訊息與分支都留著。
 * - `branches=delete`：唯一會銷毀工作成果的路徑 → `danger` 樣式 + 紅色警示塊 +
 *   要輸入 team 短名（`i42`）才能按。checkbox 每次開啟都重設回「不刪」。
 *
 * 非終態時多一段「會先停止所有成員」的說明（刪除不像 cleanup 限終態）。
 *
 * 呼叫端**只在要開的時候才掛上這個元件**（`{open ? <TeamDeleteDialog/> : null}`）：
 * 這樣每次開啟都是全新的 state，破壞性選項不可能是「上次選的」。
 */
export function TeamDeleteDialog({ teamId, onClose }: { teamId: string; onClose: () => void }) {
  const team = useStore((s) => s.teams[teamId] ?? null)
  const worktreeRoot = useStore((s) => s.teamDetail[teamId]?.worktree_root ?? '')
  const project = useStore((s) => s.projects.find((p) => p.id === (s.teams[teamId]?.project_id ?? '')) ?? null)
  const removeTeam = useStore((s) => s.removeTeam)
  const [branches, setBranches] = useState<TeamBranchDisposal>('keep')

  if (!team) return null

  const hard = branches === 'delete'
  const terminal = TEAM_TERMINAL_PHASES.includes(team.phase)
  const shortName = `i${team.issue_number}`
  const memberCount = team.members.length

  // Sidebar 的 team 節點是 `overflow: hidden` 的，對話框在那裡面會被裁掉一角，
  // 所以一律 portal 到 body（TeamPanel 那邊本來就沒問題，共用同一條路徑比較不會漏）。
  return createPortal(
    <ConfirmDialog
      // key 讓強度切換時整個重掛：輸入到一半的確認字串不會跟著保留下來。
      key={branches}
      open
      width={hard ? 420 : 380}
      title={hard ? `永久刪除 Team #${team.issue_number} 與它的分支？` : `刪除 Team #${team.issue_number}？`}
      danger={hard}
      confirmLabel={hard ? '刪除 Team 與分支' : '刪除 Team'}
      requireText={hard ? shortName : undefined}
      requireTextLabel={hard ? `請輸入「${shortName}」以確認連同分支一起刪除` : undefined}
      body={
        <div className="team-delete-body">
          {terminal ? null : (
            <p className="confirm-note">
              這個 Team <strong>還在進行中</strong>（{TEAM_PHASE_LABEL[team.phase]}）。刪除會<strong>先停止所有成員</strong>，
              等同「中止之後再刪」。
            </p>
          )}
          <p>
            會停掉 {memberCount} 個成員的 pane、移除 worktree（
            <code>{worktreeRoot || '資料目錄下的 team 目錄'}</code>）、關掉 workspace，並刪除這個 Team 的紀錄
            （task 看板與時間軸事件）。
          </p>
          <p>
            <strong>對話訊息會保留</strong>——你跟 agent 講過的話不會被刪，只是不再掛在這個 Team 底下。
          </p>
          {hard ? (
            <div className="confirm-danger" role="alert">
              <strong>分支會一起刪掉，救不回來。</strong>
              <span>
                整合分支 <code>{team.branch}</code> 與所有 task 分支會被 <code>git branch -D</code>，
                <strong>連已經合併進去的內容也會跟著消失</strong>。已經 push 到 origin 的遠端分支不會動。
              </span>
            </div>
          ) : (
            <p>
              <strong>分支會保留。</strong>整合分支 <code>{team.branch}</code> 與各 task 分支都留在 repo 裡，
              成果之後還找得回來。
            </p>
          )}
          <label className="confirm-choice">
            <input
              type="checkbox"
              checked={hard}
              onChange={(e) => setBranches(e.target.checked ? 'delete' : 'keep')}
            />
            <span>
              同時刪除分支（<code>git branch -D</code>）
              <span className="confirm-choice-sub">不勾選就只刪 Team，分支照樣留著。</span>
            </span>
          </label>
          <p className="hint">
            Team #{team.issue_number} {team.issue_title}
            {project ? ` · ${project.label}` : ''}
          </p>
        </div>
      }
      onCancel={onClose}
      onConfirm={() => {
        onClose()
        void removeTeam(teamId, branches)
      }}
    />,
    document.body,
  )
}
