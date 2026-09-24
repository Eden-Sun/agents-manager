/**
 * claude 2.1.281 的防誤刪框（SPEC §3.2、issue #423）在畫面上的樣子：
 *
 * ```
 *  │ Dangerous rm operation on statically-unresolvable target: command substitution output
 *  ⚠ Claude Code will automatically deny this request in 1:52, …
 *  Do you want to proceed?
 *  ❯ 1. Yes
 *    2. No
 * ```
 *
 * 權威的偵測在 daemon（`daemon/src/tui_prompts.rs::dangerous_rm_prompt`，它還會解出目標與指令）；
 * 這裡只判「畫面上現在是不是這個框」，用途是**把鍵盤直通鎖死**——那個框只有使用者本人能核准
 * （daemon 一個鍵都不按），不該讓一個路過的 `1` 按下去。漏判的代價只是開關仍可手動打開
 * （直通本來就預設關著），所以這裡寧可嚴格也不要亂認。
 */

/** daemon 只看畫面尾端這麼多行（非空白），這裡照抄：正文引用原文時不該被當成框。 */
const TAIL_LINES = 16

/** 框線、行首游標與大小寫都正規化掉，跟 daemon 的 `norm_line` 同一套。 */
function normLine(line: string): string {
  const spaced = Array.from(line)
    .map((c) => (/\s/.test(c) || '│┌┐└┘─├┤┬┴┼╭╮╯╰▎▔'.includes(c) ? ' ' : c.toLowerCase()))
    .join('')
  return spaced.split(/\s+/).filter(Boolean).join(' ').replace(/^[❯›»>*●•⏺⎿✻\-—\s]+/, '').trim()
}

/** 這個終端畫面現在停在防誤刪框上嗎。 */
export function isDangerousRmScreen(text: string | null | undefined): boolean {
  if (!text) return false
  const lines = text.split('\n').filter((l) => l.trim() !== '')
  const tail = lines.slice(Math.max(0, lines.length - TAIL_LINES)).map(normLine)
  // 問句在最後面、選項在問句之後、警語在問句之前：三樣都要，少一樣就不是這個框。
  const question = tail.map((l) => l.startsWith('do you want to proceed')).lastIndexOf(true)
  if (question < 0) return false
  const after = tail.slice(question + 1)
  if (!after.some((l) => l.startsWith('1. yes')) || !after.some((l) => l.startsWith('2. no'))) return false
  return tail.slice(0, question).some((l) => l.startsWith('dangerous rm operation'))
}
