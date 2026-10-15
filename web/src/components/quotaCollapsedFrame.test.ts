import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./quotaStrip.css', import.meta.url), 'utf8')

/** 取出「選擇器 {…}」那條規則的內容（只認行首的選擇器，避免 `.main-head .quota-hp` 被當成 `.quota-hp`）。 */
function ruleBody(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const m = css.match(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'm'))
  assert.ok(m, `quotaStrip.css 要有 ${selector} 規則`)
  return m[1]
}

/** #1780：收合後仍填滿剩餘寬，不能把省下的空間留在 agy 右邊。 */
test('收合格均分剩餘寬，量過的內容寬是下限', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-hp')
  assert.match(body, /flex:\s*1 0 0;/)
  assert.match(body, /min-width:\s*var\(--quota-cell-min, max-content\);/)
})

test('收合量表隨格子增長，最窄仍保留 24px', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-bar-wrap')
  assert.match(body, /flex:\s*1 0 24px;/)
  assert.match(body, /min-width:\s*24px;/)
})

test('收合的圖示欄跟內容等寬：左內距只剩格子自己的 7px（不再有 3.2em 的空欄）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-head')
  assert.match(body, /width:\s*auto;/)
  assert.match(body, /padding-right:\s*8px;/, '右邊留 8px 給窗口名，不貼著 cc0')
})
