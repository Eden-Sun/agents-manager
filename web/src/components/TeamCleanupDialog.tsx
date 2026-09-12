import { createPortal } from 'react-dom'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * 「清理 Team」的確認流程。TeamPanel 標題列與 Sidebar 的 team 節點共用（同 `TeamDeleteDialog`）。
 *
 * 清理會把成員 bot 軟刪、移除 worktree——跟刪除一樣不可逆，所以兩個入口都要先過這一關；
 * 側欄那顆 ✕ 原本直接送 `cleanup`，hover 浮出來誤點一下成員就沒了
 * （docs/reviews/2026-09-12/web.md §1 TeamNodes）。
 *
 * 呼叫端只在要開的時候才掛上（`{open ? <TeamCleanupDialog/> : null}`），跟 TeamDeleteDialog 同一套。
 */
export function TeamCleanupDialog({ teamId, onClose }: { teamId: string; onClose: () => void }) {
  const team = useStore((s) => s.teams[teamId] ?? null)
  const worktreeRoot = useStore((s) => s.teamDetail[teamId]?.worktree_root ?? '')
  const project = useStore((s) => s.projects.find((p) => p.id === (s.teams[teamId]?.project_id ?? '')) ?? null)
  const controlTeam = useStore((s) => s.controlTeam)

  if (!team) return null

  // 側欄清單自己會裁切（`.sidebar-scroll` 的 overflow），對話框掛在節點裡會被切掉一角，所以一律 portal 到 body。
  return createPortal(
    <ConfirmDialog
      open
      title="清理這個 Team？"
      body={
        <>
          <p>
            會移除 {team.members.length} 個成員 bot 與它們的 worktree（<code>{worktreeRoot || '資料目錄下的 team 目錄'}</code>）。
            <strong>分支一律保留</strong>，訊息歷史也保留。
          </p>
          <p className="hint">
            Team #{team.issue_number} {team.issue_title}
            {project ? ` · ${project.label}` : ''}
          </p>
        </>
      }
      confirmLabel="清理"
      danger
      onCancel={onClose}
      onConfirm={() => {
        onClose()
        void controlTeam(teamId, 'cleanup')
      }}
    />,
    document.body,
  )
}
