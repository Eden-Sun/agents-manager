/**
 * 終端原文裡 claude 的框線／虛線是照 pane 欄數畫的（150 欄就是 150 個 `─`／`╌`）。原文框折行顯示時
 * 一條線會折成好幾列，把畫面弄得很長又難讀（2026-10-05 console-pm 截圖：原文右邊超出視窗）。
 * 只動「很長的一整串」同一種框線字元，縮到 `keep` 個；短的裝飾線與一般文字原樣。
 */
const RULE_RUN = /([─━╌╍┄┅┈┉═])\1{39,}/g

export function fitRules(text: string, keep = 32): string {
  return text.replace(RULE_RUN, (_, ch: string) => ch.repeat(keep))
}
