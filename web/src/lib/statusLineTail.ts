/**
 * CLI 狀態列被它自己的視窗寬度截掉的尾巴（2026-09-14 使用者實拍）。
 *
 * codex 的狀態列是 `<模型> · <目錄> · Context 57% used · 5h 96% left · weekly 48% …`，pane 窄的時候
 * 它自己會在尾端補一個刪節號。我們原樣鏡射，畫面上就變成「· …」——那三個點不說任何事，卻讓人
 * 以為還有東西沒載到。
 *
 * 規則刻意保守：
 * - 整個項目都被砍掉（尾巴只剩 `· …`）→ 連那個分隔號一起去掉。
 * - 項目只被砍一半（`weekly 24%…`）→ 只去掉刪節號，`weekly 24%` 這個數字留著，它仍然有意義。
 * - 沒有刪節號就原樣回傳（真的以 `…` 結尾的訊息不在這一列出現，這裡只處理 CLI 的狀態列）。
 */
const ELLIPSIS = /(?:…|\.\.\.)\s*$/
/** 尾端那個孤零零的分隔號：codex 用 `·`，別的 CLI 可能用 `|` 或 `,`。 */
const DANGLING_SEP = /[\s]*[·|,]\s*$/

export function trimClippedTail(line: string): string {
  const s = line.trimEnd()
  if (!ELLIPSIS.test(s)) return s
  const cut = s.replace(ELLIPSIS, '').trimEnd()
  return DANGLING_SEP.test(cut) ? cut.replace(DANGLING_SEP, '').trimEnd() : cut
}
