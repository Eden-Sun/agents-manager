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

const POLL_MS = 30_000

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
