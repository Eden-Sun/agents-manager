/**
 * 「併送」：字直接打進 agent pane，不建新回合。走 `POST /bots/:id/text`（`pane.send_text`），
 * 不能用 send_keys 鍵名陣列——`\n` 不是鍵名，多行會被 herdr 擋在一半；Enter 由 daemon 另外送。
 */
export interface AlongsideIO {
  /** 回 `false` = 沒送出去（已跳通知）。 */
  sendText: (botId: string, text: string, enter: boolean) => Promise<boolean>
}

/** `true` = 已送出，可清輸入框。 */
export async function typeAlongside(io: AlongsideIO, botId: string, body: string): Promise<boolean> {
  const text = body.trim()
  if (!text) return false
  return io.sendText(botId, text, true)
}
