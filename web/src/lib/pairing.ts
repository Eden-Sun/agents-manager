/**
 * LAN 配對（SPEC §7.1a）的純函式：碼怎麼正規化、怎麼顯示，以及三種失敗各要說什麼。
 * 拆出來是因為畫面兩處（配對畫面、環境設定的產碼區）要用同一套字，而這些又是唯一值得釘測試的部分。
 */
import { ApiError } from '../api/types'
import type { PairCode } from '../api/types'

/** 六碼，字母表拿掉會唸錯的 I／O／0／1（對齊 daemon 的 `pairing::ALPHABET`）。 */
export const PAIR_CODE_LEN = 6

/** 使用者會照著唸、照著打：大小寫、空白與連字號都不算（對齊 daemon 的 `pairing::normalize`）。 */
export function normalizePairCode(raw: string): string {
  return raw.replace(/[^0-9A-Za-z]/g, '').toUpperCase()
}

/** `ABC-DEF`：唸起來有停頓；長度不對就原樣吐回去，不要自作聰明補字。 */
export function formatPairCode(raw: string): string {
  const code = normalizePairCode(raw)
  return code.length === PAIR_CODE_LEN ? `${code.slice(0, 3)}-${code.slice(3)}` : code
}

function errorCode(e: unknown): string {
  return e instanceof ApiError && typeof e.body.error === 'string' ? e.body.error : ''
}

/** `GET /api/session` 說這台裝置還沒配對——不是「連不上 daemon」，要給配對畫面。 */
export function isPairingRequired(e: unknown): boolean {
  return e instanceof ApiError && e.status === 403 && errorCode(e) === 'pairing_required'
}

/** 429 的 `retry_after_secs`（秒，無條件進位）；不是限流就 `null`，欄位缺了就 0＝「稍後再試」。 */
export function pairRetryAfterSecs(e: unknown): number | null {
  if (!(e instanceof ApiError) || e.status !== 429) return null
  const raw = e.body.retry_after_secs
  const n = typeof raw === 'number' ? raw : Number(raw)
  return Number.isFinite(n) && n > 0 ? Math.ceil(n) : 0
}

/**
 * 還要等多久（429 用）。daemon 給的是秒，而鎖定是十分鐘——「請等 600 秒」沒人讀得動，
 * 所以一分鐘以上換算成分鐘（無條件進位，寧可多等一下也不要叫人白按），不到一分鐘才講秒。
 */
export function waitText(secs: number): string {
  const s = Math.max(0, Math.ceil(secs))
  return s < 60 ? `${s} 秒` : `${Math.ceil(s / 60)} 分鐘`
}

/**
 * 碼還剩多久到期。這裡跟 `waitText` 相反，要看得到秒在動：五分鐘的碼寫「5 分鐘」
 * 會整整五分鐘都不變，使用者無從判斷還來不來得及走到手機前面。
 */
export function expiryText(secs: number): string {
  const s = Math.max(0, Math.ceil(secs))
  return s < 60 ? `${s} 秒` : `${Math.floor(s / 60)} 分 ${s % 60} 秒`
}

/**
 * 碼現在還剩幾秒。以 `expires_at` 為準而不是自己數秒：分頁被切到背景時計時器會被凍住，
 * 回來就停在一個錯的數字上。daemon 算不出 `expires_at` 才退回發碼當下的 `expires_in_secs`。
 */
export function pairCodeSecsLeft(info: PairCode, issuedAtMs: number, nowMs: number): number {
  const at = info.expires_at ? Date.parse(info.expires_at) : Number.NaN
  const endMs = Number.isFinite(at) ? at : issuedAtMs + info.expires_in_secs * 1000
  return Math.max(0, Math.ceil((endMs - nowMs) / 1000))
}

/** 429 的說法。倒數中的畫面每秒重算一次，所以獨立成一條，兩邊用同一句。 */
export function rateLimitText(secs: number): string {
  return secs > 0 ? `猜太多次了，請 ${waitText(secs)}後再試。` : '猜太多次了，請稍後再試。'
}

/**
 * 輸入配對碼失敗要說的話。碼不對／過期／用過在 daemon 那邊就是同一種回答（不透露是哪一種），
 * 所以這裡也只有一句。
 */
export function pairErrorText(e: unknown): string {
  const wait = pairRetryAfterSecs(e)
  if (wait !== null) return rateLimitText(wait)
  return '碼不正確或已失效，請在本機重新產生一個。'
}

/** 產碼失敗要說的話。`loopback_only` 是最容易撞到的一種：在手機上按了產碼按鈕。 */
export function pairCodeErrorText(e: unknown): string {
  if (e instanceof ApiError && e.status === 403 && errorCode(e) === 'loopback_only') {
    return '只有在這台機器上的瀏覽器才產得出來——請到跑 daemon 的那台開 127.0.0.1:7788，或在那台下 `agm pair-code`。'
  }
  return e instanceof Error ? e.message : String(e)
}
