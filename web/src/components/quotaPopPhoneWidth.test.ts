import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./quotaStrip.css', import.meta.url), 'utf8')

const phoneIdx = css.indexOf('  .quota-pop {\n    position: fixed;')
const phoneBody = phoneIdx >= 0 ? css.slice(phoneIdx, css.indexOf('}', phoneIdx)) : ''

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

test('手機斷點之後沒有頂層 .quota-pop 再寫死 width，蓋掉手機的等寬', () => {
  const after = css.slice(phoneIdx + phoneBody.length)
  assert.doesNotMatch(after, /\n\.quota-pop \{[^}]*\bwidth:\s*\d+px/, '窄版 300px 要包在 width > 640px 裡')
  assert.match(after, /@media \(width > 640px\) \{\n  \.quota-pop \{\n    width: 300px;/)
})
