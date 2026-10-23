import { msg } from '../lib/i18n'

/**
 * `delivery=unknown` 的提示文案（issue #1479）。
 *
 * unknown＝字打進去、鍵按了，但同步窗口內證明不了它進了 agent。daemon 會在背景繼續看證據（回音列／transcript／
 * agent 開始動作），看到就升成 `ok`；所以 bot 明顯在跑（`agent_status=working`）時，這只是「還沒確認」，不是「卡住」：
 * 用中性的字，不叫使用者「放棄該回合」（那會打斷一個其實在跑的回合）。bot 閒著、輸入框仍鎖著才是真的卡住，
 * 那時才給「放棄該回合」。
 */

/** bot 閒著（或讀不到狀態）：真的卡住，維持原本的提示。 */
export const UNKNOWN_STUCK_REASON = msg('上一回合的送達狀態未知，請先「放棄該回合」或停止 Bot')

/** bot 正在跑：送達尚未確認。 */
export const UNKNOWN_WORKING_REASON = msg('Bot 正在處理，送達尚未確認；確認後輸入框會自動解鎖')

/** 這一則剛送出、回應就是 unknown 時的提示（回應當下 `agent_status` 還沒推到網頁，不拿它分流）。 */
export const UNKNOWN_SENT_NOTICE = msg(
  '訊息已送出，送達尚未確認（delivery=unknown）：確認 bot 收到後輸入框會自動解鎖；若 bot 一直閒著、輸入框仍鎖著，才需要「放棄該回合」。',
)

/** 群組送出：有幾顆 bot 回 unknown。 */
export const UNKNOWN_GROUP_NOTICE = msg(
  '部分訊息送達尚未確認（delivery=unknown）：確認 bot 收到後會自動解鎖；若某顆 bot 一直閒著、輸入框仍鎖著，才需要「放棄該回合」。',
)

/** composer 鎖住的原因：`working` 為真時是「還在確認」，該回合的「放棄」按鈕不當成第一個選項。 */
export function unknownLock(agentStatus: string | null | undefined): { reason: string; working: boolean } {
  return agentStatus === 'working'
    ? { reason: UNKNOWN_WORKING_REASON, working: true }
    : { reason: UNKNOWN_STUCK_REASON, working: false }
}
