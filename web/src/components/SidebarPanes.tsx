import { useEffect, useRef, useState } from 'react'
import { usePollWhileVisible } from '../hooks/usePollWhileVisible'
import { setPaneAlias, type CloseNeedsConfirm, type ProjectPane } from '../api'
import { ConfirmDialog } from './ConfirmDialog'
import { ServicePaneNote } from './ServicePaneNote'
import { unownedRows } from '../lib/paneLists'
import type { UnownedTag } from '../lib/paneLists'
import { paneHint, paneLabel, paneCommand } from '../lib/panes'
import { paneReadOnly } from '../lib/shellAccess'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { isImeEnter } from '../lib/ime'
import './sidebarPanes.css'

/** pane 清單跟專案頁同一份（store），每 30 秒重讀：pane 不是每秒在變的東西，daemon 那邊本來就每輪掃描。 */
const POLL_MS = 30_000

/**
 * 選單（側欄）裡每個專案被 trace 的 shell／服務 pane（SPEC §6.5e）。
 *
 * 使用者 2026-09-16：「這 shell pane 要在 menu 可點選進入」。過去專案頁的「其他 pane」區塊管看細節、聚焦與關閉（已移除）。
 * 這裡只做一件事——**點一下就在這個 app 裡打開那顆 pane**，手機上也進得去
 * （herdr 的聚焦只動得了那台機器的 TUI）。
 *
 * 有 listen port 的 pane（dev server 之類）只能看不能打字——界線在 daemon（`shell::allowed`），面板也照著鎖住。
 * 只列 `project_id` 是這個專案的；對不到專案的在側欄底部（`SidebarUnownedPanes`）。
 */
export function SidebarPanes({ projectId }: { projectId: string }) {
  const t = useT()
  const panes = useStore((s) => s.sidePanes[projectId])
  if (!panes || panes.length === 0) return null
  return (
    <div className="side-panes" role="group" aria-label={t('這個專案的 shell pane')}>
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
  const t = useT()
  const panes = useStore((s) => s.unownedPanes)
  const refresh = useStore((s) => s.refreshPanes)

  useEffect(() => {
    void refresh()
  }, [refresh])
  usePollWhileVisible(() => void refresh(), POLL_MS)

  if (panes.length === 0) return null
  return (
    <div className="side-panes unowned" role="group" aria-label={t('沒有歸屬專案的 pane')}>
      {unownedRows(panes).map(({ pane, tag }) => (
        <PaneRow key={`${pane.host}:${pane.pane_id}`} pane={pane} tag={tag} />
      ))}
    </div>
  )
}

function PaneRow({ pane: p, tag }: { pane: ProjectPane; tag: UnownedTag }) {
  const t = useT()
  const viewPane = useStore((s) => s.viewPane)
  const here = useStore((s) => s.shellView?.host === p.host && s.shellView.paneId === p.pane_id)
  const readOnly = paneReadOnly(p)
  const service = p.kind === 'service'
  const hostNote = p.host !== 'local' ? t('（{host}）', { host: p.host }) : ''
  const tagTitle = tag === 'scratch'
    ? t('對不到專案的那顆固定 scratch，不會被自動關')
    : tag === 'extra'
      ? t('對不到任何專案、也不是 scratch：閒置太久會被自動關')
      : ''
  const tagText = tag === 'scratch' ? 'scratch' : tag === 'extra' ? t('多出來的') : ''
  const paneTitle = `${paneHint(p)}${hostNote}${readOnly ? t('（開著 port，只能看）') : ''}${tagTitle ? ` · ${tagTitle}` : ''}`
  // 關閉鍵（使用者 2026-11-04：選單要能快速關 pane）：一鍵關；在 listen 的服務 pane／讀不到狀態的，daemon 擋下來才問一次。
  const endHostShell = useStore((s) => s.endHostShell)
  const refreshPanes = useStore((s) => s.refreshPanes)
  const closing = useStore((s) => s.busy[`shell:${p.host}:${p.pane_id}`] === true)
  const [menu, setMenu] = useState(false)
  const [editing, setEditing] = useState(false)
  const [alias, setAlias] = useState('')
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)
  const savingRef = useRef(false)
  const cancelledRef = useRef(false)
  useEffect(() => {
    if (editing) {
      inputRef.current?.focus()
      inputRef.current?.select()
    }
  }, [editing])
  const startEditing = () => {
    cancelledRef.current = false
    setAlias(p.alias || paneLabel(p))
    setError('')
    setEditing(true)
  }
  const saveAlias = async () => {
    if (savingRef.current || cancelledRef.current) return
    savingRef.current = true
    setSaving(true)
    setError('')
    try {
      await setPaneAlias(p.host, p.pane_id, alias)
      await refreshPanes()
      setEditing(false)
    } catch (e) {
      setError(String(e))
    } finally {
      savingRef.current = false
      setSaving(false)
    }
  }
  const [needs, setNeeds] = useState<CloseNeedsConfirm | null>(null)
  const close = (confirm: boolean) =>
    void endHostShell(p.host, p.pane_id, confirm).then((n) => {
      if (n) setNeeds(n)
      else void refreshPanes()
    })

  return (
    <div className="side-pane-wrap">
    <div
      className={`side-pane${here ? ' current' : ''}${service ? ' service' : ''}`}
      title={paneTitle}
      onClick={() => viewPane(p)}
    >
      <button type="button" className="side-pane-view" aria-label={paneLabel(p)} aria-current={here ? 'true' : undefined} title={paneTitle} />
      <span className="side-pane-icon" aria-hidden="true">
        {service ? '⚙' : '›_'}
      </span>
      <span className="side-pane-label">
        {editing ? (
          <input ref={inputRef} className="side-pane-alias-input" aria-label={t('別名')} value={alias} maxLength={120} disabled={saving}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => setAlias(e.target.value)}
            onBlur={() => void saveAlias()}
            onKeyDown={(e) => {
              e.stopPropagation()
              if (isImeEnter(e.nativeEvent)) return
              if (e.key === 'Enter') { e.preventDefault(); void saveAlias() }
              if (e.key === 'Escape') { e.preventDefault(); cancelledRef.current = true; setEditing(false); setError('') }
            }} />
        ) : <button type="button" className="side-pane-name" aria-label={`${t('設定別名')}：${paneLabel(p)}`} title={t('設定別名')} onClick={(e) => { e.stopPropagation(); startEditing() }}>{paneLabel(p)}</button>}
        {p.alias && <small className="side-pane-detail">{paneCommand(p)}{p.listen_ports.map(port => ` :${port}`).join(',')}</small>}
      </span>
      {tag ? <span className={`side-pane-tag ${tag}`}>{tagText}</span> : service ? <span className="side-pane-tag">{t('服務')}</span> : null}
    </div>
    <button
      type="button"
      className="side-pane-close"
      disabled={closing}
      title={t('關閉這個 pane：{label}', { label: paneLabel(p) })}
      aria-label={t('關閉這個 pane')}
      onClick={() => close(false)}
    >
      ×
    </button>
    <button type="button" className="side-pane-close side-pane-more" aria-label={t('pane 選單')} aria-expanded={menu} onClick={() => setMenu(!menu)}>⋯</button>
    {menu && <div className="side-pane-menu">
      <button type="button" onClick={() => { setMenu(false); close(false) }}>{t('關閉這個 pane')}</button>
    </div>}
    {error && <p className="side-pane-alias-error" role="alert">{error}</p>}
    <ConfirmDialog
      open={needs !== null}
      title={t('這顆 pane 還在做事')}
      body={<ServicePaneNote needs={needs} />}
      confirmLabel={t('仍要關掉')}
      danger
      onCancel={() => setNeeds(null)}
      onConfirm={() => {
        setNeeds(null)
        close(true)
      }}
    />
    </div>
  )
}
