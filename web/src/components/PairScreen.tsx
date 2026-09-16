import { useEffect, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import * as api from '../api'
import { MOCK_MODE } from '../api'
import { PAIR_CODE_LEN, normalizePairCode, pairErrorText, pairRetryAfterSecs, rateLimitText } from '../lib/pairing'
import { useStore } from '../store/store'
import './pairing.css'

/**
 * 這台裝置還沒配對時的畫面（SPEC §7.1a）。
 *
 * 為什麼要有：`GET /api/session` 只對 loopback 發 token，手機走區網／Tailscale 一定收到
 * 403 `pairing_required`。沒有這個畫面，使用者在手機上看到的就是白畫面或一句「無法連上 daemon」——
 * 而 daemon 其實好好的，只是要一個碼。
 */
export function PairScreen() {
  const bootstrap = useStore((s) => s.bootstrap)
  const [code, setCode] = useState('')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  /** 429 給的等待秒數，一秒一秒扣完才讓按鈕活過來。 */
  const [wait, setWait] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)

  // 手機上開起來就是要打字，直接把游標放進去。
  useEffect(() => {
    inputRef.current?.focus()
  }, [])

  useEffect(() => {
    if (wait <= 0) return
    const t = setTimeout(() => setWait(wait - 1), 1000)
    return () => clearTimeout(t)
  }, [wait])

  // 打成 `abc def`、`ABCDEF`、`abc-def` 都算數——正規化的規矩跟 daemon 同一套，不要求使用者自己對齊。
  const normalized = normalizePairCode(code)
  const canSend = normalized.length === PAIR_CODE_LEN && !busy && wait === 0

  async function submit(e: FormEvent) {
    e.preventDefault()
    if (!canSend) return
    setBusy(true)
    setErr(null)
    try {
      await api.pairDevice(normalized)
      // 成功＝token 已經存在這台裝置上，照常開機；`bootstrap` 會把 `needsPairing` 收掉。
      await bootstrap()
    } catch (e2) {
      setErr(pairErrorText(e2))
      setWait(pairRetryAfterSecs(e2) ?? 0)
      inputRef.current?.select()
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="boot">
      <form className="boot-card pair-card" onSubmit={(e) => void submit(e)}>
        <h1>這台裝置還沒配對</h1>
        <p className="pair-lead">
          在跑 daemon 的那台機器上開「環境設定 → 手機配對」按「產生配對碼」（或下 <code>agm pair-code</code>），
          把六碼輸入這裡。碼五分鐘到期、用過即失效。
        </p>

        <label className="pair-label" htmlFor="pair-code-input">
          配對碼
        </label>
        <input
          id="pair-code-input"
          ref={inputRef}
          className="pair-input"
          // 手機上要跳出大鍵盤而且不要自動修正、不要自動大寫成句子的樣子。
          type="text"
          inputMode="text"
          autoCapitalize="characters"
          autoCorrect="off"
          autoComplete="one-time-code"
          spellCheck={false}
          placeholder="ABC-DEF"
          aria-describedby="pair-code-help"
          // 使用者打什麼就留什麼：邊打邊被改成大寫、被塞進連字號，游標會亂跳也很難刪。
          value={code}
          onChange={(e) => setCode(e.target.value.slice(0, 32))}
          disabled={busy}
        />
        <p id="pair-code-help" className="pair-help">
          小寫、空白、連字號都可以，照著唸出來打就好。
        </p>

        {wait > 0 ? (
          <p className="pair-error" role="alert">
            {rateLimitText(wait)}
          </p>
        ) : err ? (
          <p className="pair-error" role="alert">
            {err}
          </p>
        ) : null}

        <button type="submit" className="btn primary pair-submit" disabled={!canSend}>
          {busy ? '配對中…' : '配對'}
        </button>

        {MOCK_MODE ? <p className="pair-help">（MOCK 模式）</p> : null}
      </form>
    </div>
  )
}
