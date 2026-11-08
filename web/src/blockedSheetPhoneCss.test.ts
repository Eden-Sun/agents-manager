import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./styles.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')

/** 手機（≤640px）區塊裡，`selector {` 規則的內文；找不到回 null。 */
function phoneRule(selector: string): { body: string; at: number } | null {
  const media = /@media \(width <= 640px\)\s*\{/g
  for (let m = media.exec(css); m; m = media.exec(css)) {
    const start = m.index + m[0].length
    const end = css.indexOf('\n}\n', start)
    const block = css.slice(start, end)
    const i = block.indexOf(`${selector} {`)
    if (i >= 0) return { body: block.slice(i, block.indexOf('}', i)), at: start + i }
  }
  return null
}

test('手機＋選單（is-menu）是底部 sheet：貼底、高度跟內容走、最多 85%，不是全螢幕', () => {
  const sheet = phoneRule('.modal.blocked-modal.is-menu')
  assert.ok(sheet, '手機區塊要有 .modal.blocked-modal.is-menu')
  assert.match(sheet.body, /align-self:\s*flex-end/)
  assert.match(sheet.body, /height:\s*auto/)
  assert.match(sheet.body, /max-height:\s*calc\(var\(--vvh, 100dvh\)\s*\*\s*0\.85\)/)
  assert.doesNotMatch(sheet.body, /100dvh;/)
})

test('is-menu 的 sheet 規則蓋過通用的全螢幕 blocked-modal（後宣告、特異性較高）', () => {
  const full = phoneRule('.modal.blocked-modal')
  const sheet = phoneRule('.modal.blocked-modal.is-menu')
  assert.ok(full && sheet)
  assert.ok(sheet.at > full.at, 'is-menu 規則要排在全螢幕規則之後')
})

test('is-menu 的 sheet 裡只有 body 捲動，標題與底部列不縮', () => {
  assert.ok(phoneRule('.modal.blocked-modal.is-menu > *'), '子元素 flex: none')
  const body = phoneRule('.modal.blocked-modal.is-menu > .blocked-modal-body')
  assert.ok(body, 'body 另外規則')
  assert.match(body.body, /flex:\s*0 1 auto/)
})
