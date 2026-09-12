/**
 * 這一則 user 訊息其實是使用者「轉述」總管的話嗎（2026-09-12 使用者：「算在 AGM 訊息不算在 user」）。
 *
 * AGM 自己經由 relay 送的訊息有 `messages.relay_from`，畫法早就分開了；但使用者常常是自己把
 * AGM 的裁示、交辦、回覆整段貼進對話——那種 `relay_from` 是空的，於是總管的決定長得跟他自己的
 * 指示一模一樣，回頭讀對話分不出哪句是誰說的。
 *
 * 只看第一行的開頭：`AGM …：`、`[AGM …]`、`AG Man …：` 才算。句子中間提到 AGM（「問一下 AGM」）
 * 不算——寧可漏認，也不要把使用者自己的話標成別人的。
 */
/** `AGM 裁示：…`、`AGM 回覆 3407527 重建申請：…`：開頭是 AGM，24 字內就進入冒號。 */
const AGM_COLON_RE = /^\s*(AGM|AG ?Man)\b[^\n]{0,24}?[：:]/
/** `[AGM 轉交，來自 agents-manager-qn0ssg] …`：整段被方括號框起來的轉交抬頭。 */
const AGM_BRACKET_RE = /^\s*\[\s*(AGM|AG ?Man)\b[^\]\n]{0,60}\]/

export function isAgmQuote(content: string): boolean {
  const first = content.trimStart().split('\n', 1)[0] ?? ''
  return AGM_COLON_RE.test(first) || AGM_BRACKET_RE.test(first)
}
