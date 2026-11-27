import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const sidebar = readFileSync(new URL('./sidebar.css', import.meta.url), 'utf8')
const styles = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

test('主機數據區獨佔側欄頂部第二列、整列寬，不再擠在品牌與分頁晶片之間的中欄', () => {
  const m = sidebar.match(/^\.head-ram\s*\{([^}]*)\}/m)
  assert.ok(m)
  assert.match(m[1], /grid-row:\s*2/)
  assert.match(m[1], /grid-column:\s*1 \/ -1/)
  assert.match(m[1], /flex-wrap:\s*wrap/)
})

test('RAM 膠囊單行不換行，剩餘量不再強制獨佔一行', () => {
  const badge = styles.match(/^\.head-ram \.mem-badge\s*\{([^}]*)\}/m)
  assert.ok(badge)
  assert.match(badge[1], /flex-wrap:\s*nowrap/)
  assert.match(badge[1], /white-space:\s*nowrap/)
  const free = styles.match(/^\.head-ram \.mem-free\s*\{([^}]*)\}/m)
  assert.ok(free)
  assert.doesNotMatch(free[1], /100%/)
})
