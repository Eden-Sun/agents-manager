import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./quotaStrip.css', import.meta.url), 'utf8')

/** 手機（≤640px）的額度 popover 規則：`position: fixed` 那一條，必須掛在 640 斷點裡。 */
const phoneIdx = css.indexOf('  .quota-pop {\n    position: fixed;')
const phoneBody = phoneIdx >= 0 ? css.slice(phoneIdx, css.indexOf('}', phoneIdx)) : ''

/** 2026-10-09 使用者：手機按額度的 pop up 要全螢幕寬，左右貼齊視窗、只讓出 safe-area。 */
test('手機斷點的額度 popover 寬度等於視窗（左右貼齊、扣 safe-area）', () => {
  assert.ok(phoneIdx > 0, 'quotaStrip.css 要有手機 .quota-pop（position: fixed）規則')
  const media = css.slice(0, phoneIdx).lastIndexOf('@media')
  assert.match(css.slice(media, phoneIdx), /^@media \(width <= 640px\)/, '手機 .quota-pop 要在 ≤640px 斷點裡')
  assert.match(phoneBody, /left:\s*env\(safe-area-inset-left,\s*0px\);/)
  assert.match(phoneBody, /right:\s*env\(safe-area-inset-right,\s*0px\);/)
  assert.doesNotMatch(phoneBody, /\bleft:\s*8px;/, '不能再留 8px 邊')
  assert.doesNotMatch(phoneBody, /\bright:\s*8px;/, '不能再留 8px 邊')
  assert.match(phoneBody, /width:\s*auto;/)
  assert.match(phoneBody, /max-height:\s*min\(60dvh,\s*480px\);/, '內容超出時在 max-height 內捲動')
})

test('桌機的額度 popover 仍是 absolute 336px，不受手機規則影響', () => {
  const desktopIdx = css.indexOf('\n.quota-pop {\n  position: absolute;')
  assert.ok(desktopIdx > 0, '桌機 .quota-pop 規則要存在')
  const desktopBody = css.slice(desktopIdx, css.indexOf('}', desktopIdx))
  assert.match(desktopBody, /width:\s*336px;/)
  // 頂層規則不縮排；@media 裡的規則都縮排兩格，所以這條在頂層就代表不在任何斷點裡。
  assert.ok(css.startsWith('.quota-pop {', desktopIdx + 1), '桌機 .quota-pop 要是頂層規則')
})
