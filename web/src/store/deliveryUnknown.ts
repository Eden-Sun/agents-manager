import { msg } from '../lib/i18n'

export const UNKNOWN_STUCK_REASON = msg('上一回合的送達狀態未知，請先「放棄該回合」或停止 Bot')

export const UNKNOWN_WORKING_REASON = msg('Bot 正在處理，送達尚未確認；確認後輸入框會自動解鎖')

export const UNKNOWN_SENT_NOTICE = msg(
  '訊息已送出，送達尚未確認（delivery=unknown）：確認 bot 收到後輸入框會自動解鎖；若 bot 一直閒著、輸入框仍鎖著，才需要「放棄該回合」。',
)

export function unknownLock(agentStatus: string | null | undefined): { reason: string; working: boolean } {
  return agentStatus === 'working'
    ? { reason: UNKNOWN_WORKING_REASON, working: true }
    : { reason: UNKNOWN_STUCK_REASON, working: false }
}
