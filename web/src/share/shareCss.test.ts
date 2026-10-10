import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./share.css', import.meta.url), 'utf8')

/** 第一個 `selector {` 規則的內文。`from` 之後開始找。 */
function ruleBody(selector: string, from = 0): string {
  const i = css.indexOf(`${selector} {`, from)
  assert.ok(i >= 0, `share.css 找不到 ${selector}`)
  return css.slice(i, css.indexOf('}', i))
}

test('待送附件的檔名可以在膠囊裡折行', () => {
  const body = ruleBody('.sh-pending li > span:first-child')
  assert.match(body, /min-width:\s*0/)
  assert.match(body, /overflow-wrap:\s*anywhere/)
})

test('膠囊裡的按鈕不被擠扁', () => {
  const body = ruleBody('.sh-pending button')
  assert.match(body, /flex:\s*none/)
})

test('膠囊不超過一行寬', () => {
  const body = ruleBody('.sh-pending li')
  assert.match(body, /max-width:\s*100%/)
})
