import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { identityStatusOfHost, useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { copyText } from '../lib/copyText'
import { canSubmitCode, loginHint, loginPhase } from '../lib/loginAssist'
import { holdLoginPane } from '../lib/loginPaneHold'
import './claudeLoginAssist.css'

const POLL_MS = 2000

export function ClaudeLoginAssist({ host, paneId }: { host: string; paneId: string }) {
  const t = useT()
  const [status, setStatus] = useState<api.LoginStatus | null>(null)
  const [isLogin, setIsLogin] = useState(false)
  const [ended, setEnded] = useState(false)
  const [code, setCode] = useState('')
  const [sending, setSending] = useState(false)
  const [failure, setFailure] = useState<string | null>(null)
  const seen = useRef(false)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const identity = status?.identity ?? ''
  const identityStatus = useStore((s) => (identity ? identityStatusOfHost(s, host)[identity] : undefined))
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)

  useEffect(() => {
    let stop = false
    let timer: ReturnType<typeof setTimeout> | undefined
    seen.current = false
    let release: (() => void) | undefined
    const tick = async () => {
      try {
        const s = await api.fetchLoginStatus(host, paneId)
        if (stop) return
        if (s) {
          if (!seen.current) dropShellKeyboard()
          seen.current = true
          release ??= holdLoginPane(host, paneId)
          setIsLogin(true)
          setStatus(s)
        } else if (seen.current) {
          setEnded(true)
          void refreshTools(host)
          return
        } else {
          return
        }
      } catch {
      }
      if (!stop) timer = setTimeout(() => void tick(), POLL_MS)
    }
    void tick()
    return () => {
      stop = true
      if (timer) clearTimeout(timer)
      release?.()
    }
  }, [host, paneId, refreshTools])

  if (!isLogin) return null
  const phase = loginPhase(status, ended)
  const where = host === 'local' ? t('本機') : host

  async function submit() {
    if (!canSubmitCode(phase, code, sending)) return
    setSending(true)
    setFailure(null)
    try {
      const out = await api.submitLoginCode(host, paneId, code.trim())
      setCode('')
      if (out.outcome === 'failed') setFailure(out.message ?? t('CLI 回報登入失敗'))
      else if (out.outcome === 'finished') setEnded(true)
    } catch (e) {
      const msg = e instanceof ApiError && typeof e.body.message === 'string' ? e.body.message : e instanceof Error ? e.message : t('送不出去')
      setFailure(msg)
      notify('error', msg)
    } finally {
      setSending(false)
    }
  }

  const device = status?.flow === 'device'
  return (
    <div className="login-assist" role="group" aria-label={t('登入 {identity}', { identity: identity || (device ? 'codex' : status?.kind === 'agy' ? 'agy' : 'claude') })}>
      <div className="login-assist-title">
        {device
          ? t.x('登入 codex 身份 {identity}（{where}）', { identity: <strong>{identity || '…'}</strong>, where })
          : status?.kind === 'agy'
            ? t.x('登入 agy 身份 {identity}（{where}）', { identity: <strong>{identity || '…'}</strong>, where })
            : t.x('登入 claude 身份 {identity}（{where}）', { identity: <strong>{identity || '…'}</strong>, where })}
      </div>
      {phase === 'ended' ? (
        <div className="login-assist-done" role="status">
          {identityStatus?.logged_in === true
            ? identityStatus.account
              ? t('已登入（{account}）', { account: identityStatus.account })
              : t('已登入')
            : identityStatus?.logged_in === false
              ? t('登入程序已結束，但這個身份還是未登入——重新按「登入」再試一次。')
              : t('登入程序已結束；這台主機問不出登入狀態，請以實際使用為準。')}
          <button type="button" className="mini-btn" disabled={toolsBusy} onClick={() => void refreshTools(host)}>
            {toolsBusy ? t('偵測中…') : t('重新偵測')}
          </button>
        </div>
      ) : device ? (
        <>
          {status?.url ? (
            <a className="btn primary login-assist-open" href={status.url} target="_blank" rel="noopener noreferrer">
              {t('打開登入網站')}
            </a>
          ) : (
            <button type="button" className="btn primary login-assist-open" disabled>
              {t('打開登入網站')}
            </button>
          )}
          <DeviceCode code={status?.device_code ?? null} />
          <div className="login-assist-hint" role="status">
            {loginHint(phase, code)}
          </div>
        </>
      ) : (
        <>
          {status?.url ? (
            <a className="btn primary login-assist-open" href={status.url} target="_blank" rel="noopener noreferrer">
              {t('打開登入網站')}
            </a>
          ) : (
            <button type="button" className="btn primary login-assist-open" disabled>
              {t('打開登入網站')}
            </button>
          )}
          <form
            className="login-assist-form"
            onSubmit={(e) => {
              e.preventDefault()
              void submit()
            }}
          >
            <input
              className="login-assist-code"
              type="text"
              inputMode="text"
              autoComplete="off"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              placeholder={t('貼上網站給的 code')}
              aria-label={t('登入 code')}
              value={code}
              disabled={sending || phase === 'submitted'}
              onChange={(e) => setCode(e.target.value)}
            />
            <button type="submit" className="btn" disabled={!canSubmitCode(phase, code, sending)}>
              {sending ? t('送出中…') : t('送出 code')}
            </button>
          </form>
          <div className={`login-assist-hint${failure || phase === 'failed' ? ' is-error' : ''}`} role="status">
            {failure ?? (phase === 'failed' ? status?.failure : null) ?? loginHint(phase, code)}
          </div>
        </>
      )}
    </div>
  )
}

function dropShellKeyboard() {
  if (typeof window === 'undefined' || !window.matchMedia?.('(pointer: coarse)').matches) return
  const el = document.activeElement
  if (el instanceof HTMLElement && el.closest('.shell-pane')) el.blur()
}

function DeviceCode({ code }: { code: string | null }) {
  const t = useT()
  const [copied, setCopied] = useState<'idle' | 'ok' | 'fail'>('idle')
  if (!code) return <div className="login-assist-hint" role="status">{t('等 CLI 印出一次性碼…')}</div>
  return (
    <div className="login-assist-device">
      <div className="login-assist-devcode" aria-label={t('一次性碼')}>
        {code}
      </div>
      <button
        type="button"
        className="btn login-assist-copy"
        onClick={() => {
          copyText(code).then(
            (ok) => setCopied(ok ? 'ok' : 'fail'),
            () => setCopied('fail'),
          )
        }}
      >
        {copied === 'ok' ? t('已複製') : copied === 'fail' ? t('複製失敗，請選取上面的碼') : t('複製碼')}
      </button>
    </div>
  )
}

