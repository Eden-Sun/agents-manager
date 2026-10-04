import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { identityStatusOfHost, useStore } from '../store/store'
import { canSubmitCode, loginHint, loginPhase } from '../lib/loginAssist'
import './claudeLoginAssist.css'

const POLL_MS = 2000

/**
 * claude 身分的登入 pane 上方的協助條（#838）：「打開登入網站」（daemon 從登入畫面取出的 OAuth 網址，新分頁打開）＋
 * code 輸入框（daemon 確認畫面還在等 code 才打進 pane 並按 Enter）。手機上不必選取終端裡的長網址、也不必把 code 貼回終端。
 * 這顆 pane 不是 daemon 開的登入 pane（一般 shell、別種 CLI 的登入）就什麼都不畫。CLI 結束後 pane 會被 daemon 收掉，
 * 之後顯示重新偵測後的登入狀態。網址與 code 不存任何地方（不進 store、不進 localStorage）。
 */
export function ClaudeLoginAssist({ host, paneId }: { host: string; paneId: string }) {
  const [status, setStatus] = useState<api.LoginStatus | null>(null)
  /** 這顆 pane 是不是登入 pane（第一次問到才知道；404＝不是）。 */
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
    const tick = async () => {
      try {
        const s = await api.fetchLoginStatus(host, paneId)
        if (stop) return
        if (s) {
          seen.current = true
          setIsLogin(true)
          setStatus(s)
        } else if (seen.current) {
          // 看過、現在 404：CLI 結束、daemon 收掉了 pane。重驗登入狀態讓身分列更新。
          setEnded(true)
          void refreshTools(host)
          return
        } else {
          return // 從頭就不是登入 pane：不再問。
        }
      } catch {
        /* 暫時讀不到（連線抖一下）：下一輪再問 */
      }
      if (!stop) timer = setTimeout(() => void tick(), POLL_MS)
    }
    void tick()
    return () => {
      stop = true
      if (timer) clearTimeout(timer)
    }
  }, [host, paneId, refreshTools])

  if (!isLogin) return null
  const phase = loginPhase(status, ended)
  const where = host === 'local' ? '本機' : host

  async function submit() {
    if (!canSubmitCode(phase, code, sending)) return
    setSending(true)
    setFailure(null)
    try {
      const out = await api.submitLoginCode(host, paneId, code.trim())
      setCode('')
      if (out.outcome === 'failed') setFailure(out.message ?? 'CLI 回報登入失敗')
      else if (out.outcome === 'finished') setEnded(true)
    } catch (e) {
      // 409 not_awaiting_code：daemon 說了人話（畫面不在等 code，一個字都沒送）。
      const msg = e instanceof ApiError && typeof e.body.message === 'string' ? e.body.message : e instanceof Error ? e.message : '送不出去'
      setFailure(msg)
      notify('error', msg)
    } finally {
      setSending(false)
    }
  }

  return (
    <div className="login-assist" role="group" aria-label={`登入 ${identity || 'claude'}`}>
      <div className="login-assist-title">
        登入 claude 身分 <strong>{identity || '…'}</strong>（{where}）
      </div>
      {phase === 'ended' ? (
        <div className="login-assist-done" role="status">
          {identityStatus?.logged_in === true
            ? `已登入${identityStatus.account ? `（${identityStatus.account}）` : ''}`
            : identityStatus?.logged_in === false
              ? '登入程序已結束，但這個身分還是未登入——重新按「登入」再試一次。'
              : '登入程序已結束；這台主機問不出登入狀態，請以實際使用為準。'}
          <button type="button" className="mini-btn" disabled={toolsBusy} onClick={() => void refreshTools(host)}>
            {toolsBusy ? '偵測中…' : '重新偵測'}
          </button>
        </div>
      ) : (
        <>
          {status?.url ? (
            <a className="btn primary login-assist-open" href={status.url} target="_blank" rel="noopener noreferrer">
              打開登入網站
            </a>
          ) : (
            <button type="button" className="btn primary login-assist-open" disabled>
              打開登入網站
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
              placeholder="貼上網站給的 code"
              aria-label="登入 code"
              value={code}
              disabled={sending || phase === 'submitted'}
              onChange={(e) => setCode(e.target.value)}
            />
            <button type="submit" className="btn" disabled={!canSubmitCode(phase, code, sending)}>
              {sending ? '送出中…' : '送出 code'}
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
