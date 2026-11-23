import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./quotaStrip.css', import.meta.url), 'utf8')

function ruleBody(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const m = css.match(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'm'))
  assert.ok(m, `quotaStrip.css 要有 ${selector} 規則`)
  return m[1]
}

test('收合格按各自內容寬為下限（不再強制跟最寬格等寬）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-hp')
  assert.match(body, /flex:\s*1 0 0;/)
  assert.match(body, /min-width:\s*max-content;/)
})

test('收合列仍放不下時可水平捲動且帶邊緣漸層提示', () => {
  const body = ruleBody('.main-head .quota-open.collapsed')
  assert.match(body, /overflow-x:\s*auto;/)
  assert.match(body, /scrollbar-width:\s*none;/)
  const fadeRight = ruleBody(".main-head .quota-open.collapsed[data-fade-right='1']")
  assert.match(fadeRight, /mask-image:/)
})

test('收合量表隨格子增長，最窄仍保留 24px', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-bar-wrap')
  assert.match(body, /flex:\s*1 0 24px;/)
  assert.match(body, /min-width:\s*24px;/)
})

test('收合的圖示欄跟內容等寬：左內距只剩格子自己的 7px（不再有 3.2em 的空欄）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-head')
  assert.match(body, /width:\s*auto;/)
  assert.match(body, /padding-right:\s*2px;/, '右邊留 2px 加上 bar 左距 3px，緊湊不貼著身份名')
})
