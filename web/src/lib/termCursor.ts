/**
 * 終端畫面上「游標大概在哪」（2026-09-17 使用者：鍵盤直通要有閃爍游標提示）。
 * herdr 0.8.2 的 pane 讀取不回游標位置，所以用畫面推：最後一行有字的行尾就是 shell 在等輸入的地方。
 * 提示字元（`%`、`$`、`#`、`>`、`❯`）後面補一格：終端的行尾空白讀回來會被截掉，但 shell 的游標在空白後面。
 */
export function splitAtCursor(text: string): { before: string; gap: string; after: string } {
  const lines = text.split('\n')
  let last = lines.length - 1
  while (last >= 0 && !lines[last].trim()) last--
  if (last < 0) return { before: '', gap: '', after: text }
  const line = lines[last].replace(/\s+$/, '')
  const before = [...lines.slice(0, last), line].join('\n')
  const after = lines.length - 1 > last ? '\n' + lines.slice(last + 1).join('\n') : ''
  const gap = /[%$#>❯]$/.test(line) ? ' ' : ''
  return { before, gap, after }
}
