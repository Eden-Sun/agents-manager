import { useEffect, useMemo } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { useT } from '../lib/i18n'
import { loginPromptText, pendingForPrune, pendingLogins, visibleLogins } from '../lib/loginPrompt'
import { useStore } from '../store/store'
import './loginPromptBanner.css'

export function LoginPromptBanner() {
  const t = useT()
  const input = useStore(
    useShallow((s) => ({
      bots: s.bots,
      projects: s.projects,
      hosts: s.hosts,
      localIdentityStatus: s.localIdentityStatus,
      localConnected: s.connected,
    })),
  )
  const dismissed = useStore((s) => s.loginPromptDismissed)
  const dismiss = useStore((s) => s.dismissLoginPrompt)
  const prune = useStore((s) => s.pruneLoginPromptDismissed)
  const loginIdentity = useStore((s) => s.loginIdentity)
  const busy = useStore((s) => s.busy)
  const pending = useMemo(() => pendingLogins(input), [input])
  useEffect(() => prune(pendingForPrune(input, pending, dismissed)), [input, pending, dismissed, prune])
  const shown = visibleLogins(pending, dismissed)
  if (shown.length === 0) return null
  return (
    <div className="login-prompt-banner" role="alert">
      {shown.map((p) => {
        const text = loginPromptText(p)
        const working = busy[`identity-login:${p.host}:${p.identity}`] === true
        return (
          <div className="lpb-row" key={`${p.host}/${p.identity}`}>
            <span className="lpb-icon" aria-hidden="true">
              ⚠
            </span>
            <span className="lpb-text">
              <b>{text.title}</b>
              <span className="lpb-detail">{text.detail}</span>
            </span>
            <button
              type="button"
              className="btn primary lpb-login"
              disabled={working}
              title={t('在 {host} 替 {identity} 開登入：打開登入網站、貼回 code', { host: p.host === 'local' ? t('本機') : p.host, identity: p.identity })}
              onClick={() => void loginIdentity(p.host, p.identity)}
            >
              {working ? t('開啟中…') : t('立即登入')}
            </button>
            <button
              type="button"
              className="btn lpb-dismiss"
              title={t('這一次先不提示（這個畫面；身份又被登出時會再提示）')}
              onClick={() => dismiss(p.host, p.identity, p.episode)}
            >
              {t('先關掉')}
            </button>
          </div>
        )
      })}
    </div>
  )
}
