import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./styles.css', import.meta.url), 'utf8')

function ruleBody(selector: string, from = 0): string {
  const i = css.indexOf(`${selector} {`, from)
  assert.ok(i >= 0, `styles.css 找不到 ${selector}`)
  return css.slice(i, css.indexOf('}', i))
}

const zOf = (body: string) => Number(/z-index:\s*(\d+)/.exec(body)?.[1])

test('手機的通知疊在全螢幕 sheet 與確認框之上', () => {
  const modal = zOf(ruleBody('.modal-backdrop'))
  const confirm = zOf(ruleBody('.confirm-backdrop'))
  const mobileStart = css.indexOf('@media (width <= 640px)')
  assert.ok(mobileStart > 0)
  const notices = zOf(ruleBody('.notices', mobileStart))
  assert.ok(Number.isFinite(modal) && Number.isFinite(confirm) && Number.isFinite(notices))
  assert.ok(notices > modal, `手機 .notices(${notices}) 要高於 .modal-backdrop(${modal})`)
  assert.ok(notices > confirm, `手機 .notices(${notices}) 要高於 .confirm-backdrop(${confirm})`)
})

test('手機可輸入欄位的 font-size 是 16px !important', () => {
  const i = css.indexOf("input:not([type='checkbox'], [type='radio'], [type='file'], [type='range']),")
  assert.ok(i > 0, '找不到手機的欄位字級規則')
  const body = css.slice(i, css.indexOf('}', i))
  assert.match(body, /font-size:\s*16px !important;/)
  const media = css.slice(0, i).match(/@media \(width <= (\d+)px\)(?![\s\S]*@media \(width <= \d+px\))/)
  assert.equal(Number(media?.[1]), 640)
})
