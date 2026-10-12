import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

// 手機橫捲晶片列的 ◂ ▸ 是觸控目標：WCAG 2.2 AA（2.5.8）最小 24×24 CSS px。
// 寬度要照手機的左右留白（`.unread-bar` 的 padding-inline: 24px）放得進去，所以定在 24px。
const css = readFileSync(new URL('./unreadChip.css', import.meta.url), 'utf8')

test('手機的橫捲箭頭寬度 ≥ 24px（觸控目標下限），且放得進左右留白', () => {
  const rules = [...css.matchAll(/\.unread-bar-arrow\s*\{/g)]
  const phone = rules.filter((m) => {
    const media = css.lastIndexOf('@media', m.index)
    return media >= 0 && /max-width:\s*720px/.test(css.slice(media, css.indexOf('{', media)))
  })
  assert.ok(phone.length > 0, '手機斷點要有 .unread-bar-arrow 的規則')
  const body = css.slice(phone[0].index, css.indexOf('}', phone[0].index))
  const width = Number(/width:\s*(\d+)px/.exec(body)?.[1])
  assert.equal(width, 24, `手機 .unread-bar-arrow 寬度應為 24px，現在是 ${width}`)
})
