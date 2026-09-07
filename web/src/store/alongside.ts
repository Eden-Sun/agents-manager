/**
 * 「併送」——把使用者打的字直接打進 agent 的 pane，不建新回合（等同他自己在終端裡輸入）。
 *
 * 抽成一支吃 IO 的純函式，就是為了讓「多行文字要完整到達」測得到。舊寫法把整段文字拆成
 * **鍵名**陣列送 `agent.send_keys`（空白換成 `space`），而 `\n` 不是任何一顆鍵的名字：
 * 多行內容送到一半就被 herdr 擋掉，使用者只看到前半段進了輸入框。
 *
 * 所以文字走 `POST /bots/:id/text`（daemon 端是 `pane.send_text`），Enter 由 daemon 在文字
 * 之後**另外**用 `pane.send_keys` 送——`\n` 在 `pane.send_text` 裡是貼上的換行，不是送出。
 */
export interface AlongsideIO {
  /** `enter` = 打完字接一個 Enter。回 `false` = 沒送出去（原因已經跳通知）。 */
  sendText: (botId: string, text: string, enter: boolean) => Promise<boolean>
}

/** `true` = 字已經出去了（呼叫端可以清輸入框）；`false` = 沒東西可送，或送失敗。 */
export async function typeAlongside(io: AlongsideIO, botId: string, body: string): Promise<boolean> {
  const text = body.trim()
  if (!text) return false
  return io.sendText(botId, text, true)
}
