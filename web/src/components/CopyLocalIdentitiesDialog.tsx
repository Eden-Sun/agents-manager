import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import * as api from '../api'
import type { IdentitySyncItem, IdentitySyncPlan, IdentitySyncResponse } from '../api/types'
import { useT } from '../lib/i18n'
import { isImeEnter } from '../lib/ime'
import { plainErrMsg } from '../lib/errText'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useStore } from '../store/store'
import './identitiesPanel.css'

export interface CopyLocalIdentitiesDialogProps {
  open: boolean
  host: string
  onClose: () => void
}

type Phase = 'loading' | 'ready' | 'applying' | 'done' | 'failed'

const defaultSelection = (plan: IdentitySyncPlan): Set<string> =>
  new Set(plan.items.filter((i) => i.status === 'missing' && i.can_apply).map((i) => i.name))

const envLines = (env: Record<string, string>): string =>
  Object.entries(env).map(([k, v]) => `${k}=${v}`).join('\n')

export function CopyLocalIdentitiesDialog({ open, host, onClose }: CopyLocalIdentitiesDialogProps) {
  const t = useT()
  const titleId = useId()
  const dialogRef = useRef<HTMLDivElement>(null)
  const refreshState = useStore((s) => s.refreshState)
  const refreshTools = useStore((s) => s.refreshTools)
  const loginIdentity = useStore((s) => s.loginIdentity)
  const [phase, setPhase] = useState<Phase>('loading')
  const [plan, setPlan] = useState<IdentitySyncPlan | null>(null)
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [response, setResponse] = useState<IdentitySyncResponse | null>(null)
  const [error, setError] = useState('')
  const [reload, setReload] = useState(0)
  const [lastOpen, setLastOpen] = useState(open)
  if (lastOpen !== open) {
    setLastOpen(open)
    if (open) {
      setPhase('loading')
      setError('')
      setResponse(null)
    }
  }

  useDialogFocus(open, dialogRef, {
    initialFocus: () => dialogRef.current?.querySelector<HTMLInputElement>('input[type=checkbox]:not(:disabled)') ?? null,
  })

  useEffect(() => {
    if (!open) return
    let alive = true
    api.getIdentitySyncPlan(host).then(
      (p) => {
        if (!alive) return
        setPlan(p)
        setSelected(defaultSelection(p))
        setPhase('ready')
      },
      (e: unknown) => {
        if (!alive) return
        setError(plainErrMsg(e))
        setPhase('failed')
      },
    )
    return () => {
      alive = false
    }
  }, [open, host, reload])

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || isImeEnter(e)) return
      const dialog = dialogRef.current
      if (dialog && e.target instanceof Node && !dialog.contains(e.target)) return
      e.preventDefault()
      e.stopPropagation()
      if (phase !== 'applying') onClose()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, phase, onClose])

  if (!open) return null

  const items: IdentitySyncItem[] = plan?.items ?? []
  const applicable = items.filter((i) => i.status === 'missing' && i.can_apply)
  const chosen = applicable.filter((i) => selected.has(i.name))

  const toggle = (name: string, on: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (on) next.add(name)
      else next.delete(name)
      return next
    })
  }

  const apply = async () => {
    if (chosen.length === 0 || phase !== 'ready') return
    setPhase('applying')
    try {
      const res = await api.syncIdentitiesToHost(host, chosen.map((i) => i.name))
      setResponse(res)
      setPhase('done')
      await refreshState()
      await refreshTools(host)
    } catch (e) {
      setError(plainErrMsg(e))
      setPhase('failed')
    }
  }

  const resultLabel = (status: string) => {
    switch (status) {
      case 'created': return t('已建立')
      case 'would_create': return t('將建立')
      case 'skipped': return t('已略過')
      default: return t('失敗')
    }
  }

  const body = (() => {
    if (phase === 'loading') return <p className="copy-identities-status" role="status">{t('載入中…')}</p>
    if (phase === 'failed' && !plan) {
      return (
        <div className="copy-identities-error" role="alert">
          <p>{t('無法載入可複製的身份：{reason}', { reason: error })}</p>
          <div className="copy-identities-actions">
            <button
              type="button"
              className="btn primary"
              onClick={() => {
                setPhase('loading')
                setError('')
                setReload((n) => n + 1)
              }}
            >
              {t('重新載入')}
            </button>
          </div>
        </div>
      )
    }
    if (phase === 'done' && response) {
      return (
        <div className="copy-identities-done">
          <ul className="copy-identities-results">
            {response.results.map((r) => (
              <li key={r.name} className={`copy-identities-result is-${r.status}`} data-result={r.name}>
                <span className="identity-name">{r.name}</span>
                <span className="copy-identities-tag">{resultLabel(r.status)}</span>
                {r.message || r.reason ? <span className="copy-identities-note">{r.message ?? r.reason}</span> : null}
                {r.status === 'created' ? (
                  <button
                    type="button"
                    className="mini-btn primary"
                    aria-label={t('登入 {host} 的 {name}', { host, name: r.name })}
                    onClick={() => void loginIdentity(host, r.name, { background: true })}
                  >
                    {t('登入')}
                  </button>
                ) : null}
              </li>
            ))}
          </ul>
          {response.backup_path ? (
            <p className="copy-identities-hint">
              {t('已備份原設定檔：')} <code>{response.backup_path}</code>
            </p>
          ) : null}
          <p className="copy-identities-hint">{t('建立後在遠端為未登入狀態，請逐一按「登入」完成授權。')}</p>
          <div className="copy-identities-actions">
            <button type="button" className="btn primary" onClick={onClose}>{t('完成')}</button>
          </div>
        </div>
      )
    }
    if (!plan) return null
    if (items.length === 0) {
      return (
        <>
          <p className="copy-identities-status" role="status">
            {t('{host} 已擁有本機的所有可複製身份（已同步 {n} 項）', { host, n: plan.already_synced })}
          </p>
          <div className="copy-identities-actions">
            <button type="button" className="btn primary" onClick={onClose}>{t('關閉')}</button>
          </div>
        </>
      )
    }
    return (
      <>
        <p className="copy-identities-explain">
          {t('只複製設定與 shell alias（路徑用 $HOME），不複製憑證、token 或登入檔。建立後在遠端為未登入狀態，可逐一登入。')}
          {plan.remote_rc ? ` ${t('shell alias 會寫入 {rc}。', { rc: plan.remote_rc })}` : ''}
        </p>
        <ul className="copy-identities-list" aria-label={t('可複製的身份')}>
          {items.map((i) => {
            const disabled = !(i.status === 'missing' && i.can_apply)
            return (
              <li key={i.name} className={`copy-identities-row is-${i.status}`} data-copy-identity={i.name}>
                <div className="copy-identities-head">
                  <label className="copy-identities-pick">
                    <input
                      type="checkbox"
                      checked={!disabled && selected.has(i.name)}
                      disabled={disabled || phase === 'applying'}
                      onChange={(e) => toggle(i.name, e.target.checked)}
                    />
                    <span className="identity-name">{i.name}</span>
                  </label>
                  <span className="copy-identities-tag">{i.kind}</span>
                  <span className="copy-identities-tag">{i.source === 'shell' ? t('shell alias') : t('config')}</span>
                  <span className={`copy-identities-status-tag is-${i.status}`}>
                    {i.status === 'missing' ? t('缺漏') : t('設定不同')}
                  </span>
                </div>
                <pre className="copy-identities-env">{i.alias_line ?? (envLines(i.local_env) || t('（無 env）'))}</pre>
                {i.status === 'different' && i.remote_env ? (
                  <pre className="copy-identities-env is-remote">{t('遠端：')}{'\n'}{envLines(i.remote_env) || t('（無 env）')}</pre>
                ) : null}
                {i.note ? <p className="copy-identities-note">{i.note}</p> : null}
              </li>
            )
          })}
        </ul>
        {plan.already_synced > 0 ? (
          <p className="copy-identities-hint">{t('已同步 {n} 項，不需要處理。', { n: plan.already_synced })}</p>
        ) : null}
        {phase === 'failed' ? (
          <p className="copy-identities-error" role="alert">{t('建立失敗：{reason}', { reason: error })}</p>
        ) : null}
        <div className="copy-identities-actions">
          <button type="button" className="btn" onClick={onClose} disabled={phase === 'applying'}>{t('取消')}</button>
          <button type="button" className="btn primary" onClick={() => void apply()} disabled={chosen.length === 0 || phase === 'applying'}>
            {phase === 'applying' ? t('建立中…') : t('建立所選身份（{n}）', { n: chosen.length })}
          </button>
        </div>
      </>
    )
  })()

  return createPortal(
    <div className="confirm-backdrop" role="presentation" onMouseDown={() => { if (phase !== 'applying') onClose() }}>
      <div
        className="copy-identities-dialog"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="copy-identities-title">{t('從本機複製身份到 {host}', { host })}</h2>
        {body}
      </div>
    </div>,
    document.body,
  )
}
