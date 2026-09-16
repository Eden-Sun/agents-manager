import { useEffect, useState } from 'react'
import * as api from '../api'
import type { PairCode } from '../api/types'
import { expiryText, pairCodeErrorText, pairCodeSecsLeft } from '../lib/pairing'
import './pairing.css'

/**
 * 環境設定 → 手機配對（SPEC §7.1a）。
 *
 * 為什麼一定要有這顆按鈕：daemon 擋下非 loopback 的 `GET /api/session` 時，回的那句話直接叫使用者
 * 「在本機的環境設定按『產生配對碼』」。沒有這一塊，那句話就是在叫人去找一個不存在的東西。
 *
 * 產碼本身只有 loopback 做得到——把整個 API 的權限交出去的人，必須已經站在這台機器前面。
 * 所以在手機上開這一頁按下去會收到 403 `loopback_only`，那不是壞掉，要好好講清楚。
 */
export function PairCodeBox() {
  const [info, setInfo] = useState<PairCode | null>(null)
  /** 拿到碼的時刻；daemon 沒給 `expires_at` 時用它加 `expires_in_secs` 補算。 */
  const [issuedAt, setIssuedAt] = useState(0)
  const [left, setLeft] = useState(0)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  // 每秒重算而不是自己減一：分頁被切到背景時計時器會被凍住，回來就停在錯的數字上。
  useEffect(() => {
    if (!info) return
    const tick = () => setLeft(pairCodeSecsLeft(info, issuedAt, Date.now()))
    tick()
    const t = setInterval(tick, 1000)
    return () => clearInterval(t)
  }, [info, issuedAt])

  async function issue() {
    setBusy(true)
    setErr(null)
    try {
      const got = await api.issuePairCode()
      setInfo(got)
      setIssuedAt(Date.now())
    } catch (e) {
      setErr(pairCodeErrorText(e))
      setInfo(null)
      setLeft(0)
    } finally {
      setBusy(false)
    }
  }

  const live = info !== null && left > 0

  return (
    <div className="pair-code">
      <p className="pair-code-hint">
        手機或另一台電腦第一次打開這個網址時要輸入一次配對碼。碼五分鐘到期、<strong>用過即失效</strong>，
        配對成功之後那台裝置就不用再輸入了。
      </p>

      {info ? (
        <>
          <output className="pair-code-value">{info.code}</output>
          <p className={`pair-code-left${live ? '' : ' expired'}`}>
            {live ? `${expiryText(left)}後到期` : '這個碼已經到期了，再產生一個。'}
          </p>
        </>
      ) : null}

      {err ? <p className="pair-code-err">{err}</p> : null}

      <button type="button" className="btn" onClick={() => void issue()} disabled={busy}>
        {busy ? '產生中…' : info ? '再產生一個' : '產生配對碼'}
      </button>
    </div>
  )
}
