import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const sidebar = readFileSync(new URL('./sidebar.css', import.meta.url), 'utf8')
const styles = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

test('主機數據區每台主機單行不換行，建置晶片與本機並排不另佔一列', () => {
  const m = sidebar.match(/^\.head-host-row\s*\{([^}]*)\}/m)
  assert.ok(m)
  assert.match(m[1], /flex-wrap:\s*nowrap/)
  const chip = sidebar.match(/^\.head-host-row > \.build-slots-chip\s*\{([^}]*)\}/m)
  assert.ok(chip)
  assert.match(chip[1], /margin-left:\s*auto/)
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
