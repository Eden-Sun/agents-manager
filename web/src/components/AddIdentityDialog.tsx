import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { BotKind, Identity, NewIdentityInput } from '../api/types'
import { useT } from '../lib/i18n'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { isImeEnter } from '../lib/ime'
import {
  IDENTITY_KINDS,
  checkEnvText,
  identityHomeDir,
  identityHomeVar,
  sharedHomeWith,
  suggestedIdentityEnvText,
  validIdentityName,
} from './identityEnv'
import './addIdentityDialog.css'

export interface AddIdentityHost {
  name: string
  label: string
  connected: boolean
}

export interface AddIdentityDialogProps {
  open: boolean
  hosts: AddIdentityHost[]
  initialHost?: string
  initialKind?: BotKind
  existing: readonly Identity[]
  onCreate: (input: NewIdentityInput, host: string) => Promise<boolean>
  onLogin: (host: string, name: string) => Promise<boolean>
  onOpenTerminal?: () => void
  loggedIn: (host: string, name: string) => boolean | null
  refreshStatus: () => Promise<void>
  recheck?: (host: string, name: string) => Promise<unknown>
  onCreated?: (host: string, name: string) => void
  onClose: () => void
  pollMs?: number
  maxChecks?: number
}

type Phase = 'form' | 'creating' | 'opening' | 'waiting' | 'checking' | 'done' | 'failed'

interface KindPrompt {
  from: BotKind
  to: BotKind
}

export function AddIdentityDialog({
  open,
  hosts,
  initialHost,
  initialKind,
  existing,
  onCreate,
  onLogin,
  onOpenTerminal,
  loggedIn,
  refreshStatus,
  recheck,
  onCreated,
  onClose,
  pollMs = 3000,
  maxChecks = 40,
}: AddIdentityDialogProps) {
  const t = useT()
  const titleId = useId()
  const dialogRef = useRef<HTMLDivElement>(null)
  const nameRef = useRef<HTMLInputElement>(null)
  const [host, setHost] = useState(initialHost ?? hosts[0]?.name ?? 'local')
  const [kind, setKind] = useState<BotKind>(initialKind ?? 'claude')
  const [name, setName] = useState('')
  const [envText, setEnvText] = useState(suggestedIdentityEnvText(initialKind ?? 'claude', ''))
  const [kindPrompt, setKindPrompt] = useState<KindPrompt | null>(null)
  const [phase, setPhase] = useState<Phase>('form')
  const [created, setCreated] = useState<{ host: string; name: string } | null>(null)
  const [openFailed, setOpenFailed] = useState(false)
  const [alsoCreate, setAlsoCreate] = useState(false)
  const [alsoCreated, setAlsoCreated] = useState<string[]>([])
  const attemptsRef = useRef(0)
  const [lastOpen, setLastOpen] = useState(open)
  if (lastOpen !== open) {
    setLastOpen(open)
    if (open) {
      setHost(initialHost ?? hosts[0]?.name ?? 'local')
      setKind(initialKind ?? 'claude')
      setName('')
      setEnvText(suggestedIdentityEnvText(initialKind ?? 'claude', ''))
      setKindPrompt(null)
      setPhase('form')
      setCreated(null)
      setOpenFailed(false)
      setAlsoCreate(false)
      setAlsoCreated([])
      attemptsRef.current = 0
    }
  }

  const latest = useRef({ refreshStatus, loggedIn, recheck })
  useEffect(() => {
    latest.current = { refreshStatus, loggedIn, recheck }
  })

  useDialogFocus(open, dialogRef, { initialFocus: () => nameRef.current })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || isImeEnter(e)) return
      const dialog = dialogRef.current
      if (dialog && e.target instanceof Node && !dialog.contains(e.target)) return
      e.preventDefault()
      e.stopPropagation()
      if (phase !== 'creating' && phase !== 'opening') onClose()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, phase, onClose])

  useEffect(() => {
    if (phase !== 'waiting') return
    if (attemptsRef.current >= maxChecks) {
      setPhase('failed')
      return
    }
    const timer = setTimeout(() => setPhase('checking'), pollMs)
    return () => clearTimeout(timer)
  }, [phase, pollMs, maxChecks])

  useEffect(() => {
    if (phase !== 'checking' || !created) return
    let alive = true
    attemptsRef.current += 1
    const { host: h, name: n } = created
    void (async () => {
      try {
        await latest.current.recheck?.(h, n)
      } catch {
      }
      try {
        await latest.current.refreshStatus()
      } catch {
      }
      if (!alive) return
      const st = latest.current.loggedIn(h, n)
      if (st === true) setPhase('done')
      else setPhase(attemptsRef.current >= maxChecks ? 'failed' : 'waiting')
    })()
    return () => {
      alive = false
    }
  }, [phase, created, maxChecks])

  if (!open) return null

  const nameOk = validIdentityName(name)
  const env = checkEnvText(envText)
  const homeVar = identityHomeVar(kind)
  const homeDir = identityHomeDir(kind, env.env)
  const shared = sharedHomeWith(kind, env.env, host, existing)
  const duplicate = nameOk && existing.some((i) => i.name === name && (i.host || 'local') === host)
  const hostInfo = hosts.find((h) => h.name === host)
  const hostOffline = hostInfo !== undefined && !hostInfo.connected
  const hostLabel = hostInfo?.label ?? host
  const sharedNames = shared.names.join('、')
  const blocking: string[] = []
  if (name && !nameOk) blocking.push(t('名稱：小寫英文字母開頭，之後可接小寫字母、數字、- 或 _，最多 32 個字。'))
  if (duplicate) blocking.push(t('{host} 上已經有名為 {name} 的身份。', { host: hostLabel, name }))
  if (homeVar && !homeDir) {
    blocking.push(t('env 要有 {var}（否則身份會悄悄用預設帳號）。', { var: homeVar }))
  }
  if (env.errors.length > 0) blocking.push(...env.errors)
  if (hostOffline) blocking.push(t('{host} 目前未連線，不能在那台建立身份。', { host: hostLabel }))
  const canSubmit = nameOk && blocking.length === 0 && phase === 'form'
  const otherConnected = hosts.filter((h) => h.name !== host && h.connected)
  const conflictHosts = nameOk ? otherConnected.filter((h) => existing.some((i) => i.name === name && (i.host || 'local') === h.name)) : []
  const alsoTargets = otherConnected.filter((h) => !conflictHosts.includes(h))

  const submit = async () => {
    if (!canSubmit) return
    const input: NewIdentityInput = { name, kind, env: env.env, ...(host !== 'local' ? { host } : {}) }
    setPhase('creating')
    const ok = await onCreate(input, host)
    if (!ok) {
      setPhase('form')
      return
    }
    const extra: string[] = []
    if (alsoCreate) {
      for (const h of alsoTargets) {
        const extraInput: NewIdentityInput = { name, kind, env: env.env, ...(h.name !== 'local' ? { host: h.name } : {}) }
        if (await onCreate(extraInput, h.name)) extra.push(h.name)
      }
    }
    setAlsoCreated(extra)
    setCreated({ host, name })
    onCreated?.(host, name)
    openLogin(host, name)
  }

  const openLogin = async (h: string, n: string) => {
    setPhase('opening')
    const opened = await onLogin(h, n).catch(() => false)
    setOpenFailed(!opened)
    attemptsRef.current = 0
    setPhase(opened ? 'waiting' : 'failed')
  }

  const onNameChange = (raw: string) => {
    const next = raw.toLowerCase()
    if (envText === suggestedIdentityEnvText(kind, name)) setEnvText(suggestedIdentityEnvText(kind, next))
    setName(next)
    setKindPrompt(null)
  }

  const changeKind = (next: BotKind) => {
    if (next === kind) return
    const untouched = envText === suggestedIdentityEnvText(kind, name)
    if (untouched) {
      setEnvText(suggestedIdentityEnvText(next, name))
      setKindPrompt(null)
    } else {
      setKindPrompt({ from: kind, to: next })
    }
    setKind(next)
  }

  const statusText = (() => {
    switch (phase) {
      case 'opening': return t('開啟登入終端…')
      case 'waiting': return t('等待登入：請在開啟的終端完成授權，完成後會自動檢查。')
      case 'checking': return t('檢查中…')
      case 'done': return t('已登入')
      case 'failed': return openFailed
        ? t('身份已建立，但沒能開啟登入終端。')
        : t('還沒確認到登入（最後檢查：未登入或無法確認）。完成授權後按「重新檢查」。')
      default: return ''
    }
  })()

  const showForm = phase === 'form' || (phase === 'creating' && created === null)
  const busy = phase === 'creating' || phase === 'opening' || phase === 'checking'

  return createPortal(
    <div className="confirm-backdrop" role="presentation" onMouseDown={() => { if (!busy) onClose() }} onClick={(e) => e.stopPropagation()}>
      <div
        className="add-identity-dialog"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="add-identity-title">{t('新增帳號')}</h2>

        {showForm ? (
          <form
            className="add-identity-form"
            onSubmit={(e) => {
              e.preventDefault()
              void submit()
            }}
          >
            {hosts.length > 1 ? (
              <label className="add-identity-field">
                <span>{t('主機')}</span>
                <select value={host} onChange={(e) => setHost(e.target.value)} disabled={phase === 'creating'}>
                  {hosts.map((h) => (
                    <option key={h.name} value={h.name}>
                      {h.label}{h.connected ? '' : ` (${t('未連線')})`}
                    </option>
                  ))}
                </select>
              </label>
            ) : (
              <p className="add-identity-host-only">{t('建立於本機')}</p>
            )}

            {otherConnected.length > 0 ? (
              <div className="add-identity-also">
                <label className="add-identity-also-toggle">
                  <input
                    type="checkbox"
                    checked={alsoCreate}
                    disabled={phase === 'creating'}
                    onChange={(e) => setAlsoCreate(e.target.checked)}
                  />
                  <span>{t('同時在其他已連線主機建立（{names}）', { names: otherConnected.map((h) => h.label).join('、') })}</span>
                </label>
                {nameOk && conflictHosts.length > 0 ? (
                  <ul className="add-identity-hint-list">
                    {conflictHosts.map((h) => (
                      <li key={h.name}>{t('{host} 已有同名身份，將略過', { host: h.label })}</li>
                    ))}
                  </ul>
                ) : null}
              </div>
            ) : null}

            <fieldset className="add-identity-field">
              <legend>{t('CLI')}</legend>
              <div className="add-identity-kinds" role="radiogroup" aria-label={t('CLI')}>
                {IDENTITY_KINDS.map((k) => (
                  <label key={k} className="add-identity-kind">
                    <input type="radio" name="add-identity-kind" value={k} checked={kind === k} onChange={() => changeKind(k)} />
                    {k}
                  </label>
                ))}
              </div>
            </fieldset>

            <label className="add-identity-field">
              <span>{t('名稱（例如 cc1）')}</span>
              <input
                ref={nameRef}
                type="text"
                value={name}
                placeholder="cc1"
                spellCheck={false}
                autoComplete="off"
                onChange={(e) => onNameChange(e.target.value)}
              />
            </label>

            {kindPrompt ? (
              <div className="add-identity-prompt" role="status">
                <p>
                  {t('env 是你為 {from} 填的自訂值。要保留，還是改用 {to} 的建議？', { from: kindPrompt.from, to: kindPrompt.to })}
                </p>
                <div className="add-identity-prompt-actions">
                  <button type="button" className="btn" onClick={() => setKindPrompt(null)}>
                    {t('保留自訂值')}
                  </button>
                  <button
                    type="button"
                    className="btn"
                    onClick={() => {
                      setEnvText(suggestedIdentityEnvText(kindPrompt.to, name))
                      setKindPrompt(null)
                    }}
                  >
                    {t('改用 {to} 建議', { to: kindPrompt.to })}
                  </button>
                </div>
              </div>
            ) : null}

            <details className="add-identity-advanced">
              <summary>{t('進階：env')}</summary>
              <label className="add-identity-field">
                <span>{t('每行 KEY=VALUE；{home} 會展開成該主機的家目錄。', { home: '$HOME' })}</span>
                <textarea
                  rows={3}
                  value={envText}
                  spellCheck={false}
                  onChange={(e) => {
                    setEnvText(e.target.value)
                    setKindPrompt(null)
                  }}
                />
              </label>
            </details>

            <section className="add-identity-summary" aria-label={t('摘要')}>
              <dl>
                <dt>{t('所在主機')}</dt>
                <dd>{hostLabel}</dd>
                <dt>{t('實際目錄')}</dt>
                <dd>
                  {homeDir ? <code>{homeDir}</code> : <span className="add-identity-none">{t('（未設定）')}</span>}
                </dd>
              </dl>
              {shared.names.length > 0 ? (
                <p className="add-identity-warn" role="alert">
                  {t('這個目錄已被 {names} 使用：兩個身份會共用同一份登入。', { names: sharedNames })}
                </p>
              ) : null}
              {shared.defaultAccount ? (
                <p className="add-identity-warn" role="alert">
                  {t('這個目錄就是預設帳號用的目錄：新身份會和預設帳號共用同一份登入。')}
                </p>
              ) : null}
              {blocking.length > 0 && name !== '' ? (
                <ul className="add-identity-errors" role="alert">
                  {blocking.map((b) => (
                    <li key={b}>{b}</li>
                  ))}
                </ul>
              ) : null}
            </section>

            <div className="add-identity-actions">
              <button type="button" className="btn" onClick={onClose} disabled={phase === 'creating'}>
                {t('取消')}
              </button>
              <button type="submit" className="btn primary" disabled={!canSubmit}>
                {phase === 'creating' ? t('建立中…') : t('新增並登入')}
              </button>
            </div>
          </form>
        ) : (
          <div className="add-identity-progress">
            <p className="add-identity-created">
              <strong>{created?.name}</strong> · {hosts.find((h) => h.name === created?.host)?.label ?? created?.host}
            </p>
            {alsoCreated.length > 0 ? (
              <ul className="add-identity-also-list" aria-label={t('其他已建立的主機')}>
                {alsoCreated.map((h) => (
                  <li key={h}>
                    <span>{hosts.find((x) => x.name === h)?.label ?? h}</span>
                    <button type="button" className="mini-btn" onClick={() => void onLogin(h, created?.name ?? name)}>
                      {t('登入')}
                    </button>
                  </li>
                ))}
              </ul>
            ) : null}
            <p className={`add-identity-status is-${phase}`} role="status" aria-live="polite">
              {statusText}
            </p>
            <div className="add-identity-actions">
              {phase === 'waiting' ? (
                <>
                  <button type="button" className="btn" onClick={onClose}>
                    {t('稍後再登入')}
                  </button>
                  {onOpenTerminal ? (
                    <button type="button" className="btn" onClick={onOpenTerminal}>
                      {t('打開登入終端')}
                    </button>
                  ) : null}
                  <button type="button" className="btn primary" onClick={() => setPhase('checking')}>
                    {t('我已完成登入，立即檢查')}
                  </button>
                </>
              ) : null}
              {phase === 'failed' ? (
                <>
                  <button type="button" className="btn" onClick={onClose}>
                    {t('關閉')}
                  </button>
                  {openFailed ? (
                    <button type="button" className="btn primary" onClick={() => created && void openLogin(created.host, created.name)}>
                      {t('重試登入')}
                    </button>
                  ) : onOpenTerminal ? (
                    <>
                      <button type="button" className="btn" onClick={onOpenTerminal}>
                        {t('打開登入終端')}
                      </button>
                      <button type="button" className="btn primary" onClick={() => { attemptsRef.current = 0; setPhase('checking') }}>
                        {t('重新檢查')}
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      className="btn primary"
                      onClick={() => {
                        attemptsRef.current = 0
                        setPhase('checking')
                      }}
                    >
                      {t('重新檢查')}
                    </button>
                  )}
                </>
              ) : null}
              {phase === 'done' ? (
                <button type="button" className="btn primary" onClick={onClose}>
                  {t('完成')}
                </button>
              ) : null}
            </div>
          </div>
        )}
      </div>
    </div>,
    document.body,
  )
}
