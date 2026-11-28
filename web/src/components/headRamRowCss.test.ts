import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const sidebar = readFileSync(new URL('./sidebar.css', import.meta.url), 'utf8')
const styles = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

test('主機數據區採雙欄並排（方案 3），不另佔多餘列高', () => {
  const m = sidebar.match(/^\.head-ram\s*\{([^}]*)\}/m)
  assert.ok(m)
  assert.match(m[1], /display:\s*grid/)
  const multi = sidebar.match(/^\.head-ram\.multi\s*\{([^}]*)\}/m)
  assert.ok(multi)
  assert.match(multi[1], /repeat\(2,\s*minmax\(0,\s*1fr\)\)/)
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
