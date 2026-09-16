import { useEffect } from 'react'
import { paneHint, paneLabel } from '../lib/panes'
import { paneReadOnly } from '../lib/shellAccess'
import { useStore } from '../store/store'
import './sidebarPanes.css'

/**
 * 選單（側欄）裡每個專案被 trace 的 shell／服務 pane（SPEC §6.5e）。
 *
 * 使用者 2026-09-16：「這 shell pane 要在 menu 可點選進入」。專案頁的「其他 pane」區塊（`ProjectPanes`）
 * 管的是看細節、聚焦與關閉；這裡只做一件事——**點一下就在這個 app 裡打開那顆 pane**，手機上也進得去
 * （herdr 的聚焦只動得了那台機器的 TUI）。
 *
 * 有 listen port 的 pane（dev server 之類）只能看不能打字——界線在 daemon（`shell::allowed`），面板也照著鎖住。
 */
export function SidebarPanes({ projectId }: { projectId: string }) {
  const panes = useStore((s) => s.sidePanes[projectId])
  const load = useStore((s) => s.loadSidePanes)
  const viewPane = useStore((s) => s.viewPane)
  const current = useStore((s) => s.shellView)

  // 展開專案時讀一次：pane 不是每秒在變的東西，daemon 那邊本來就每輪掃描。
  useEffect(() => {
    void load(projectId)
  }, [load, projectId])

  if (!panes || panes.length === 0) return null

  return (
    <div className="side-panes" role="list" aria-label="這個專案的 shell pane">
      {panes.map((p) => {
        const here = current?.host === p.host && current.paneId === p.pane_id
        return (
          <button
            key={`${p.host}:${p.pane_id}`}
            type="button"
            role="listitem"
            className={`side-pane${here ? ' current' : ''}${p.kind === 'service' ? ' service' : ''}`}
            title={`${paneHint(p)}${paneReadOnly(p) ? '（開著 port，只能看）' : ''}`}
            onClick={() => viewPane(p)}
          >
            <span className="side-pane-icon" aria-hidden="true">
              {p.kind === 'service' ? '⚙' : '›_'}
            </span>
            <span className="side-pane-label">{paneLabel(p)}</span>
            {p.kind === 'service' ? <span className="side-pane-tag">服務</span> : null}
          </button>
        )
      })}
    </div>
  )
}
