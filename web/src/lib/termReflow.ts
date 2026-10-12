/**
 * 把寬 pane 的終端畫面整理成手機讀得下去的樣子（2026-10-12 使用者：213 欄的「需要回應」畫面在手機上「根本看不清楚訊息內容」）。
 *
 * 寬畫面原樣折行時讀不了的原因都是「照欄數排的版」：外框把每列補到滿寬、欄位用一長串空白對齊、深縮排吃掉半個螢幕、
 * 55 列裡大半是空列。這裡只拿掉排版用的東西，字一個都不改：
 * - 行尾空白、框線列（`╭──╮`／`├──┤`）與每列兩端的框邊（`│ … │`）
 * - 行內連續三個以上的空白收成兩個（欄位仍分得開）
 * - 縮排最多留 `maxIndent` 格（階層還看得出來）
 * - 連續空列收成一列，頭尾的空列去掉
 *
 * 選項編號、游標（`❯`）、`>` 提示都在內文裡，不會被動到。寬螢幕不要呼叫這支：那裡要看的就是原樣的畫面。
 */
const FRAME_LINE = /^\s*[╭╮╰╯┌┐└┘├┤┬┴┼╔╗╚╝╠╣─━═╌╍┄┅┈┉\s]*[╭╮╰╯┌┐└┘├┤╔╗╚╝╠╣][╭╮╰╯┌┐└┘├┤┬┴┼╔╗╚╝╠╣─━═╌╍┄┅┈┉\s]*$/
const EDGE = '[│┃║]'
const LEFT_EDGE = new RegExp(`^(\\s*)${EDGE} ?`)
const RIGHT_EDGE = new RegExp(`\\s*${EDGE}$`)

export function reflowForPhone(text: string, maxIndent = 4): string {
  const out: string[] = []
  for (const raw of text.split('\n')) {
    let line = raw.replace(/\s+$/, '')
    if (FRAME_LINE.test(line)) continue
    // 兩端都有框邊才當外框拆；只有一邊的（引用線、樹狀圖）是內文。
    if (LEFT_EDGE.test(line) && RIGHT_EDGE.test(line) && line.trim().length > 1) {
      line = line.replace(RIGHT_EDGE, '').replace(LEFT_EDGE, '$1')
    } else if (/^\s*[│┃║]$/.test(line)) {
      line = ''
    }
    const indent = /^ */.exec(line)![0].length
    line = ' '.repeat(Math.min(indent, maxIndent)) + line.slice(indent).replace(/ {3,}/g, '  ')
    if (line === '' && (out.length === 0 || out[out.length - 1] === '')) continue
    out.push(line)
  }
  while (out.length && out[out.length - 1] === '') out.pop()
  return out.join('\n')
}
