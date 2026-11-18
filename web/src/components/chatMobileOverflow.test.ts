/**
 * 手機（390 寬）對話區水平超版（2026-11-18 iPhone 截圖）：訊息列表整個左右可捲，助理泡泡與時間列被切。
 * 根因：`.msg-meta` 是 nowrap 且高度固定 20px，列內子項 min-width:auto 縮不下來，譯文狀態列（「譯文跟原文幾乎一樣…」）
 * 把整列撐出泡泡、再撐寬整個列表。修法在 chatPanel.css：列與左側子項可縮，有狀態列時列可換行、長字串斷行；
 * 表格與 code block 各自在自己內部橫向捲動（不靠整體 overflow-x:hidden）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./chatPanel.css', import.meta.url), 'utf8')
const styles = await readFile(new URL('../styles.css', import.meta.url), 'utf8')

/** 取出某個選擇器的第一個規則本體（不含選擇器）。 */
function ruleBody(src: string, selector: string): string {
  const i = src.indexOf(`${selector} {`)
  assert.ok(i >= 0, `找不到規則 ${selector}`)
  return src.slice(i, src.indexOf('}', i))
}

test('譯文狀態列所在的 meta 列：有狀態列時可換行、高度跟內容走', () => {
  const body = ruleBody(css, '.msg-meta:has(.msg-translate-hint)')
  assert.match(body, /flex-wrap:\s*wrap;/)
  assert.match(body, /white-space:\s*normal;/)
  assert.match(body, /height:\s*auto;/)
})

test('meta 列與其左側子項可縮（min-width:0），不再撐寬泡泡', () => {
  assert.match(ruleBody(css, '.msg-meta-left'), /min-width:\s*0;/)
  assert.match(css, /\.msg-meta-above,\s*\.msg-meta-below,\s*\.msg-meta-left\s*\{\s*min-width:\s*0;\s*max-width:\s*100%;/)
})

test('譯文狀態列自己會斷行（長字串 overflow-wrap:anywhere）', () => {
  const body = ruleBody(css, '.msg-meta .msg-translate-hint')
  assert.match(body, /min-width:\s*0;/)
  assert.match(body, /overflow-wrap:\s*anywhere;/)
})

test('表格與 code block 在自己內部橫向捲動，不靠整體 overflow-x', () => {
  assert.match(ruleBody(styles, '.bubble.md table'), /overflow-x:\s*auto;/)
  assert.match(ruleBody(styles, '.bubble.md pre'), /overflow(-x)?:\s*auto;/)
})
