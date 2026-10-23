import { useCallback, useEffect, useState } from 'react'
import { usePollWhileVisible } from '../hooks/usePollWhileVisible'
import * as api from '../api'
import type { GhPending, GhStatus } from '../api/types'
import { t, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { CopyChip } from './CopyChip'
import { hostLabel } from './toolsHelpers'
import './ghAuth.css'

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
  const t = useT()
  if (!pending && !error) return null
  return (
    <div className="gh-device">
      {error ? <p className="gh-device-err" role="alert">{error}</p> : null}
      {pending ? (
        <>
          <p className="gh-device-lead">{t('在瀏覽器打開 GitHub，輸入這組一次性代碼：')}</p>
          <CopyChip label={t('代碼')} value={pending.user_code} title={t('GitHub 裝置碼')} />
          <a className="gh-device-link" href={pendingUrl(pending)} target="_blank" rel="noreferrer">
            {t('打開 github.com/login/device')}
          </a>
        </>
      ) : null}
      {onCancel ? (
        <button type="button" className="mini-btn" onClick={onCancel}>
          {t('取消')}
        </button>
      ) : null}
    </div>
  )
}

async function startLogin(host: string, notify: (kind: 'info' | 'error', text: string) => void): Promise<GhStatus> {
  const next = await api.loginGh(host, 'auto')
  if (next.pending) {
    window.open(pendingUrl(next.pending), '_blank', 'noopener')
    notify('info', t('{host}：請在瀏覽器完成 GitHub 授權（代碼 {code}）', { host: hostLabel(host), code: next.pending.user_code }))
  } else if (next.logged_in) {
    notify('info', t('{host} 的 gh 已登入', { host: hostLabel(host) }) + (next.account ? t('（{account}）', { account: next.account }) : ''))
  } else if (next.error) {
    notify('error', next.error)
  }
  return next
}

/** HostsPanel row: current gh account + a login button when it cannot talk to GitHub. */
export function GhHostStatus({ host }: { host: string }) {
  const t = useT()
  const notify = useStore((s) => s.notify)
  const [st, setSt] = useState<GhStatus | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = useCallback(() => {
    void api
      .fetchGhStatus(host)
      .then(setSt)
      // 讀不到就留著上一份：輪詢裝置碼授權時一次失敗不能把代碼面板收掉、把輪詢停掉。第一次就讀不到時 st 本來就是 null（照舊不畫）。
      .catch(() => undefined)
  }, [host])

  useEffect(() => {
    refresh()
  }, [refresh])

  usePollWhileVisible(refresh, st?.pending ? 2000 : null)

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
    ? t('gh 未安裝')
    : st.logged_in === true
      ? t('gh · {account}', { account: st.account ?? t('已登入') })
      : st.logged_in === null
        ? t('gh · 狀態未知')
        : st.account
          ? t('gh · {account}（未登入）', { account: st.account })
          : t('gh 未登入')
  const authState = !st.installed ? 'missing' : st.logged_in === true ? 'ok' : st.logged_in === false ? 'out' : 'unknown'
  return (
    <span className={`gh-auth ${authState}`}>
      <span
        className="gh-auth-label"
        title={st.logged_in === null ? t('登入狀態無法確認；按登入會改用裝置碼，不會複製本機憑證') : st.path ?? 'gh'}
      >
        {label}
      </span>
      {st.logged_in || !st.installed ? null : (
        <button type="button" className="mini-btn" disabled={busy || Boolean(st.pending)} onClick={() => void login()}>
          {busy ? t('登入中…') : st.pending ? t('等待授權…') : st.logged_in === null ? t('裝置碼登入') : t('登入')}
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
  const t = useT()
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

  usePollWhileVisible(refresh, st?.pending ? 2000 : null)

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
        {busy ? t('登入中…') : st?.pending ? t('等待授權…') : t('在 {host} 登入 gh', { host: hostLabel(host) })}
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
