import { ApiError } from '../api/types'
import { msg, tr } from './i18n'

export function plainErrMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

export function humanMessage(e: unknown): string | null {
  if (!(e instanceof ApiError)) return null
  const m = e.body.message
  return typeof m === 'string' && m.trim() ? m.trim() : null
}

export function isLegacyDaemonError(e: ApiError): boolean {
  return e.status === 405 || (e.status === 404 && e.body.what !== 'bot')
}

export function reasonTextBySubstring(raw: string, table: ReadonlyArray<readonly [string, string]>): string | null {
  for (const [key, text] of table) if (raw.includes(key)) return tr(text)
  return null
}

export const RETIRE_REASON_TEXT = [
  ['api_retire_busy', msg('bot 仍有進行中的回合或關閉尚未完成，請稍後再試')],
  ['api_retire_unresolved', msg('對話還有未結清的紀錄，暫時無法清除')],
  ['api_retire_unconfirmed', msg('無法確認清除結果，請重新整理後再確認')],
  ['api_retire_storage', msg('資料庫暫時無法寫入，請稍後再試')],
] as const

export const BASE_SNAPSHOT_REASON_TEXT = [
  ['base_snapshot_no_session', msg('這顆 Bot 還沒有對話可鎖定，請先跟它對話')],
  ['base_snapshot_not_codex', msg('基準快照只支援 codex、agy Bot')],
  ['base_snapshot_unsupported_kind', msg('基準快照只支援 codex、agy Bot')],
  ['base_snapshot_create_failed', msg('自動製作基準快照失敗，請確認 Bot 能正常啟動並有對話後再試')],
  ['base_snapshot_copy_failed', msg('複製 agy 對話失敗，無法建立基準快照')],
  ['base_fork_failed', msg('agy 對話分岔失敗，這次沒有在舊 context 執行')],
] as const
