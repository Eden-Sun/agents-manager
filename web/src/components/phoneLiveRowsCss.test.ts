import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./chatPanel.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')

function rule(selector: string, from = 0): string {
  const i = css.indexOf(`${selector} {`, from)
  assert.ok(i >= 0, `缺少 ${selector}`)
  return css.slice(i, css.indexOf('}', i))
}

test('進行中狀態列：活動文字可縮成省略號，「看目前內容」不縮、不被裁', () => {
  assert.match(rule('.msg.live .live-activity'), /min-width:\s*0[\s\S]*text-overflow:\s*ellipsis/)
  assert.match(rule('.msg.live .live-peek'), /flex:\s*none/)
})

test('手機 Running 橫幅：狀態字讓位給按鈕，按鈕不換行，收成一列', () => {
  const phone = css.indexOf('@media (width <= 640px)')
  const span = rule('.bot-composer .composer-lock.running > span:first-child', phone)
  assert.match(span, /flex:\s*1 1 0/)
  assert.match(span, /text-overflow:\s*ellipsis/)
  assert.match(rule('.bot-composer .composer-lock.running .mini-btn', phone), /white-space:\s*nowrap/)
})
