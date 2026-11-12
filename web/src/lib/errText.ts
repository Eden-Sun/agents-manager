import { ApiError } from '../api/types'
import { msg, tr } from './i18n'

/** 所有 errText 系列的共同底：Error 取 message，其他轉字串。 */
export function plainErrMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/** daemon 的 `message`（給人看的那一句）去頭尾空白；沒有或空白回 null。 */
export function humanMessage(e: unknown): string | null {
  if (!(e instanceof ApiError)) return null
  const m = e.body.message
  return typeof m === 'string' && m.trim() ? m.trim() : null
}

/** 405，或 404 且不是「bot 找不到」＝正在跑的 daemon 比前端舊、還沒有這支 API。 */
export function isLegacyDaemonError(e: ApiError): boolean {
  return e.status === 405 || (e.status === 404 && e.body.what !== 'bot')
}

/** 錯誤字串（常是 `ApiError.message`）含某個機器 key 就換成對照的人話；都沒中回 null。 */
export function reasonTextBySubstring(raw: string, table: ReadonlyArray<readonly [string, string]>): string | null {
  for (const [key, text] of table) if (raw.includes(key)) return tr(text)
  return null
}

/** `POST …/api-bot/retire` 的機器 key（錯誤字串內含）。 */
export const RETIRE_REASON_TEXT = [
  ['api_retire_busy', msg('bot 仍有進行中的回合或關閉尚未完成，請稍後再試')],
  ['api_retire_unresolved', msg('對話還有未結清的紀錄，暫時無法清除')],
  ['api_retire_unconfirmed', msg('無法確認清除結果，請重新整理後再確認')],
  ['api_retire_storage', msg('資料庫暫時無法寫入，請稍後再試')],
] as const

/** 基準快照的機器 key（錯誤字串內含）。 */
export const BASE_SNAPSHOT_REASON_TEXT = [
  ['base_snapshot_no_session', msg('這顆 Bot 還沒有對話可鎖定，請先跟它對話')],
  ['base_snapshot_not_codex', msg('基準快照只支援 codex Bot')],
] as const
