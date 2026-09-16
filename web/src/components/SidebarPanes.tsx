import { useEffect } from 'react'
import type { ProjectPane } from '../api'
import { unownedRows } from '../lib/paneLists'
import type { UnownedTag } from '../lib/paneLists'
import { paneHint, paneLabel } from '../lib/panes'
import { paneReadOnly } from '../lib/shellAccess'
import { useStore } from '../store/store'
import './sidebarPanes.css'

/** pane 清單跟專案頁同一份（store），每 30 秒重讀：pane 不是每秒在變的東西，daemon 那邊本來就每輪掃描。 */
const POLL_MS = 30_000

/**
 * 選單（側欄）裡每個專案被 trace 的 shell／服務 pane（SPEC §6.5e）。
 *
 * 使用者 2026-09-16：「這 shell pane 要在 menu 可點選進入」。專案頁的「其他 pane」區塊（`ProjectPanes`）
 * 管的是看細節、聚焦與關閉；這裡只做一件事——**點一下就在這個 app 裡打開那顆 pane**，手機上也進得去
 * （herdr 的聚焦只動得了那台機器的 TUI）。
 *
 * 有 listen port 的 pane（dev server 之類）只能看不能打字——界線在 daemon（`shell::allowed`），面板也照著鎖住。
 * 只列 `project_id` 是這個專案的；對不到專案的在側欄底部（`SidebarUnownedPanes`）。
 */
export function SidebarPanes({ projectId }: { projectId: string }) {
  const panes = useStore((s) => s.sidePanes[projectId])
  if (!panes || panes.length === 0) return null
  return (
    <div className="side-panes" role="list" aria-label="這個專案的 shell pane">
      {panes.map((p) => (
        <PaneRow key={`${p.host}:${p.pane_id}`} pane={p} tag={null} />
      ))}
    </div>
  )
}

/**
 * 側欄底部「開 shell」旁：對不到任何專案的 pane。scratch 固定排第一列；其他的本來就不該存在，標「多出來的」，
 * 點得進去讓人自己看完決定（SPEC §6.5e）。整個側欄的 pane 輪詢也掛在這裡——它一直都在，不隨專案收合。
 */
export function SidebarUnownedPanes() {
  const panes = useStore((s) => s.unownedPanes)
  const refresh = useStore((s) => s.refreshPanes)

  useEffect(() => {
    void refresh()
    const t = setInterval(() => void refresh(), POLL_MS)
    return () => clearInterval(t)
  }, [refresh])

  if (panes.length === 0) return null
  return (
    <div className="side-panes unowned" role="list" aria-label="沒有歸屬專案的 pane">
      {unownedRows(panes).map(({ pane, tag }) => (
        <PaneRow key={`${pane.host}:${pane.pane_id}`} pane={pane} tag={tag} />
      ))}
    </div>
  )
}

const TAG_TEXT: Record<Exclude<UnownedTag, null>, { text: string; title: string }> = {
  scratch: { text: 'scratch', title: '對不到專案的那顆固定 scratch，不會被自動關' },
  extra: { text: '多出來的', title: '對不到任何專案、也不是 scratch：閒置太久會被自動關' },
}

function PaneRow({ pane: p, tag }: { pane: ProjectPane; tag: UnownedTag }) {
  const viewPane = useStore((s) => s.viewPane)
  const here = useStore((s) => s.shellView?.host === p.host && s.shellView.paneId === p.pane_id)
  const readOnly = paneReadOnly(p)
  const service = p.kind === 'service'
  const hostNote = p.host !== 'local' ? `（${p.host}）` : ''
  return (
    <button
      type="button"
      role="listitem"
      className={`side-pane${here ? ' current' : ''}${service ? ' service' : ''}`}
      title={`${paneHint(p)}${hostNote}${readOnly ? '（開著 port，只能看）' : ''}${tag ? `・${TAG_TEXT[tag].title}` : ''}`}
      onClick={() => viewPane(p)}
    >
      <span className="side-pane-icon" aria-hidden="true">
        {service ? '⚙' : '›_'}
      </span>
      <span className="side-pane-label">{paneLabel(p)}</span>
      {tag ? <span className={`side-pane-tag ${tag}`}>{TAG_TEXT[tag].text}</span> : service ? <span className="side-pane-tag">服務</span> : null}
    </button>
  )
}
