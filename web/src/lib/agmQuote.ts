/**
 * 這一則 user 訊息其實是「別人的話」嗎，以及是誰的（2026-09-12 使用者：「算在 AGM 訊息不算在 user」、
 * 「這則訊息也不是我問的」）。
 *
 * 正規的來源標示是 `messages.relay_from`（daemon 記的）。但有兩種情況它會是空的：
 * - 使用者自己把總管的裁示整段貼進對話（`AGM 裁示：…`）；
 * - bot 之間走 `herdr agent prompt`，daemon 只看到 prompt 回音，而 `/relay/announce` 的比對沒對上
 *   （回音被截短、或送出的是多段其中一段）——那種訊息通常自帶 `[來自 <bot> / <agent>]` 抬頭。
 *
 * 兩種都只認**第一行的開頭**，句子中間提到（「問一下 AGM」）不算：寧可漏認，也不要把使用者自己的話
 * 說成別人送的。回傳顯示用的名字，`null` = 這就是使用者自己打的。
 */

/** `AGM 裁示：…`、`AGM 回覆 3407527 重建申請：…`：開頭是 AGM，24 字內就進入冒號。 */
const AGM_COLON_RE = /^\s*(AGM|AG ?Man)\b[^\n]{0,24}?[：:]/
/** `[AGM 轉交，來自 agents-manager-qn0ssg] …`：整段被方括號框起來的轉交抬頭。 */
const AGM_BRACKET_RE = /^\s*\[\s*(AGM|AG ?Man)\b[^\]\n]{0,60}\]/
/** `[來自 c1-主要功能 / agents-manager-qn0ssg]`：bot 之間互轉時自己寫的抬頭，抓 bot 名。 */
const FROM_BRACKET_RE = /^\s*\[\s*來自\s*([^/\]\n]{1,40}?)\s*(?:\/[^\]\n]{0,60})?\]/

export function quotedFrom(content: string): string | null {
  const first = content.trimStart().split('\n', 1)[0] ?? ''
  const named = FROM_BRACKET_RE.exec(first)
  if (named) return named[1]
  if (AGM_COLON_RE.test(first) || AGM_BRACKET_RE.test(first)) return 'AGM'
  return null
}

/** 舊名，保留給只想知道「是不是總管的話」的呼叫端。 */
export function isAgmQuote(content: string): boolean {
  return quotedFrom(content) === 'AGM'
}
