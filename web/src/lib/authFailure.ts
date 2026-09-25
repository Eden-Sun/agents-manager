import type { Message } from '../api/types'

/** daemon 把 `StopFailure` 分類成 `FailureReason::Auth` 時寫的系統訊息標籤（`hookrecv.rs` 的 `label()`）。 */
const HOOK_AUTH_LABEL = '（帳號或授權）'
/** claude 沒登入時回合只回這一行；2.1.28x 有 `Please run /login` 與 `Run /login` 兩種寫法。 */
const NOT_LOGGED_IN = /not logged in\s*·\s*(?:please\s+)?run \/login/i
const ONLY_NOT_LOGGED_IN = /^[⎿\s]*not logged in\s*·\s*(?:please\s+)?run \/login[.。]?\s*$/i

type Msg = Pick<Message, 'role' | 'content'>

/**
 * 這則是「回合因為沒登入／授權失敗而收尾」嗎。系統訊息是 daemon 寫的，含標籤或引用那一行就算；
 * agent 的回覆只認**整則就是那一行**（畫面備援抓到的就長這樣）——bot 在回報裡引用那句話不能長出登入鈕。
 */
export function isAuthFailure(msg: Msg): boolean {
  if (msg.role === 'system') return msg.content.includes(HOOK_AUTH_LABEL) || NOT_LOGGED_IN.test(msg.content)
  if (msg.role === 'assistant') return ONLY_NOT_LOGGED_IN.test(msg.content)
  return false
}

/**
 * 對話裡哪一則 auth 失敗要掛「立即登入」：只有最後一則，而且它之後還沒有正常的回覆
 * （有了就代表已經登入好、重送成功，按鈕留著只是雜訊）。沒有回 null。
 */
export function authActionTargetId(list: readonly (Msg & Pick<Message, 'id'>)[]): string | null {
  for (let i = list.length - 1; i >= 0; i -= 1) {
    const m = list[i]
    if (isAuthFailure(m)) return m.id
    if (m.role === 'assistant') return null
  }
  return null
}
