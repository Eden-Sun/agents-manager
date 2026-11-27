import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const read = (p: string) => readFileSync(new URL(p, import.meta.url), 'utf8')
const styles = read('./styles.css')
const quota = read('./components/quotaStrip.css')
const picker = read('./components/modelPicker.css')

test('桌機 toast 墊在輸入列（dock）上方，沒有 dock 時仍高過專案設定的 Save 列', () => {
  const m = styles.match(/(?:^|\n)\.notices\s*\{([^}]*)\}/)
  assert.ok(m)
  assert.match(m[1], /bottom:\s*calc\(var\(--composer-bottom-offset,\s*64px\)\s*\+\s*12px\)/)
})

test('額度 stale 標記是標籤左側的小點，不疊到進度條，保留 tooltip 文字', () => {
  const m = quota.match(/\.quota-window-stale\s*\{([^}]*)\}/)
  assert.ok(m)
  assert.match(m[1], /position:\s*absolute/)
  assert.match(m[1], /left:\s*-5px/)
  assert.match(m[1], /font-size:\s*0/)
  assert.match(quota, /\.quota-window-name:has\(> \.quota-window-stale\)\s*\{\s*position:\s*relative/)
})

test('active 模型鈕裡的副標用 --accent-text，不沿用深灰的 field-note 色', () => {
  assert.match(picker, /\.opt-group \.opt\.on \.field-note\s*\{\s*color:\s*var\(--accent-text\);?\s*\}/)
})
