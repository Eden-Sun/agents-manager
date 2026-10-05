/**
 * 終端原文裡 agent TUI 的分隔線是照 pane 欄數畫的（150 欄就是 150 個 `─`／`╌`，`recent_unwrapped` 更寬）。
 * 網頁折行顯示時一條線會折成好幾列，把畫面弄得很長又難讀（2026-10-05 console-pm 截圖、終端分頁輸入框上下那幾條）。
 *
 * 只動「整列只有分隔線字元（可含前後空白）而且夠長」的列，把線縮到 `keep` 個；內文、行內的裝飾線、短線原樣。
 * 所有顯示終端原文又會折行的地方都呼叫這一支（`linkifyTerm`、`BlockedModal`），不要各自複製規則。
 */
const RULE_LINE = /^(\s*)([─━╌╍┄┅┈┉═]{40,})\s*$/

export function fitRules(text: string, keep = 32): string {
  if (!/[─━╌╍┄┅┈┉═]{40}/.test(text)) return text
  return text
    .split('\n')
    .map((line) => {
      const m = RULE_LINE.exec(line)
      return m ? m[1] + m[2].slice(0, keep) : line
    })
    .join('\n')
}
