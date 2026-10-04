import { useEffect, useMemo } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { loginPromptText, pendingLogins, visibleLogins } from '../lib/loginPrompt'
import { useStore } from '../store/store'
import './loginPromptBanner.css'

/**
 * 主動提示：某個 claude 身分被登出（探測說未登入，或綁著它的 bot 回合因授權失敗收尾），而且有 bot 綁著它——
 * 主畫面最上面一條「cc1（m4p）已登出 → 立即登入」，手機也看得到，不必自己去環境設定的身分列找（#838 補充，使用者 2026-10-04）。
 * 「立即登入」走 `loginIdentity`＝打開 #838 的登入協助面板（打開登入網站＋貼 code）；登入成功（探測回已登入／daemon 清掉記號）提示自己消失。
 * 「先關掉」只對這一次登出有效、只存這一頁（`loginPromptDismissed`）。判斷都在 `lib/loginPrompt.ts`。
 */
export function LoginPromptBanner() {
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
  useEffect(() => prune(pending), [pending, prune])
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
              title={`在 ${p.host === 'local' ? '本機' : p.host} 替 ${p.identity} 開登入：打開登入網站、貼回 code`}
              onClick={() => void loginIdentity(p.host, p.identity)}
            >
              {working ? '開啟中…' : '立即登入'}
            </button>
            <button
              type="button"
              className="btn lpb-dismiss"
              title="這一次先不提示（這個畫面；身分又被登出時會再提示）"
              onClick={() => dismiss(p.host, p.identity, p.episode)}
            >
              先關掉
            </button>
          </div>
        )
      })}
    </div>
  )
}
