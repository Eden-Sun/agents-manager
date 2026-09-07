import { useCallback, useEffect, useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import type { GhPending, GhStatus } from '../api/types'
import { useStore } from '../store/store'
import { CopyChip } from './CopyChip'
import { hostLabel } from './Tools'

/** 502 from `gh issue list` when the host is not authenticated (or the active token is dead). */
export function isGhAuthError(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status !== 502) return false
  const m = e.message.toLowerCase()
  return m.includes('未登入') || m.includes('auth login') || m.includes('authentication') || m.includes('401')
}

function pendingUrl(p: GhPending): string {
  return p.verification_uri_complete || p.verification_uri || 'https://github.com/login/device'
}

function GhDevicePanel({
  pending,
  error,
  onCancel,
}: {
  pending: GhPending | null
  error: string | null
  onCancel?: () => void
}) {
  if (!pending && !error) return null
  return (
    <div className="gh-device">
      {error ? <p className="gh-device-err">{error}</p> : null}
      {pending ? (
        <>
          <p className="gh-device-lead">在瀏覽器打開 GitHub，輸入這組一次性代碼：</p>
          <CopyChip label="代碼" value={pending.user_code} title="GitHub 裝置碼" />
          <a className="gh-device-link" href={pendingUrl(pending)} target="_blank" rel="noreferrer">
            打開 github.com/login/device
          </a>
        </>
      ) : null}
      {onCancel ? (
        <button type="button" className="mini-btn" onClick={onCancel}>
          取消
        </button>
      ) : null}
    </div>
  )
}

async function startLogin(host: string, notify: (kind: 'info' | 'error', text: string) => void): Promise<GhStatus> {
  const next = await api.loginGh(host, 'auto')
  if (next.pending) {
    window.open(pendingUrl(next.pending), '_blank', 'noopener')
    notify('info', `${hostLabel(host)}：請在瀏覽器完成 GitHub 授權（代碼 ${next.pending.user_code}）`)
  } else if (next.logged_in) {
    notify('info', `${hostLabel(host)} 的 gh 已登入${next.account ? `（${next.account}）` : ''}`)
  } else if (next.error) {
    notify('error', next.error)
  }
  return next
}

/** HostsPanel row: current gh account + a login button when it cannot talk to GitHub. */
export function GhHostStatus({ host }: { host: string }) {
  const notify = useStore((s) => s.notify)
  const [st, setSt] = useState<GhStatus | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = useCallback(() => {
    void api
      .fetchGhStatus(host)
      .then(setSt)
      .catch(() => setSt(null))
  }, [host])

  useEffect(() => {
    refresh()
  }, [refresh])

  useEffect(() => {
    if (!st?.pending) return
    const t = window.setInterval(refresh, 2000)
    return () => window.clearInterval(t)
  }, [st?.pending, refresh])

  const login = async () => {
    if (busy) return
    setBusy(true)
    try {
      setSt(await startLogin(host, notify))
    } catch (e) {
      notify('error', e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  if (!st) return null
  const label = !st.installed
    ? 'gh 未安裝'
    : st.logged_in
      ? `gh · ${st.account ?? '已登入'}`
      : st.account
        ? `gh · ${st.account}（未登入）`
        : 'gh 未登入'
  return (
    <span className={`gh-auth${st.logged_in ? ' ok' : st.installed ? ' out' : ' missing'}`}>
      <span className="gh-auth-label" title={st.path ?? 'gh'}>
        {label}
      </span>
      {st.logged_in || !st.installed ? null : (
        <button type="button" className="mini-btn" disabled={busy || Boolean(st.pending)} onClick={() => void login()}>
          {busy ? '登入中…' : st.pending ? '等待授權…' : '登入'}
        </button>
      )}
      {st.pending || st.error ? (
        <GhDevicePanel
          pending={st.pending}
          error={st.error}
          onCancel={
            st.pending
              ? () => {
                  void api
                    .cancelGhLogin(host)
                    .then(setSt)
                    .catch(() => undefined)
                }
              : undefined
          }
        />
      ) : null}
    </span>
  )
}

/** IssuesBar error row: one-click login for the project's host, then refetch. */
export function GhLoginButton({ host, onLoggedIn }: { host: string; onLoggedIn: () => void }) {
  const notify = useStore((s) => s.notify)
  const [st, setSt] = useState<GhStatus | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = useCallback(() => {
    void api
      .fetchGhStatus(host)
      .then((next) => {
        setSt(next)
        if (next.logged_in) onLoggedIn()
      })
      .catch(() => undefined)
  }, [host, onLoggedIn])

  useEffect(() => {
    if (!st?.pending) return
    const t = window.setInterval(refresh, 2000)
    return () => window.clearInterval(t)
  }, [st?.pending, refresh])

  const login = async () => {
    if (busy) return
    setBusy(true)
    try {
      const next = await startLogin(host, notify)
      setSt(next)
      if (next.logged_in) onLoggedIn()
    } catch (e) {
      notify('error', e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="gh-login-cta">
      <button type="button" className="mini-btn primary" disabled={busy || Boolean(st?.pending)} onClick={() => void login()}>
        {busy ? '登入中…' : st?.pending ? '等待授權…' : `在 ${hostLabel(host)} 登入 gh`}
      </button>
      {st?.pending || st?.error ? (
        <GhDevicePanel
          pending={st.pending}
          error={st.error}
          onCancel={
            st.pending
              ? () => {
                  void api
                    .cancelGhLogin(host)
                    .then(setSt)
                    .catch(() => undefined)
                }
              : undefined
          }
        />
      ) : null}
    </div>
  )
}
