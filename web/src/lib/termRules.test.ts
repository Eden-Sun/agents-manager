import test from 'node:test'
import assert from 'node:assert/strict'
import { fitRules } from './termRules.ts'

test('整列都是很長的分隔線：縮短，前面的縮排保留、後面的空白去掉', () => {
  assert.equal(fitRules('╌'.repeat(150)), '╌'.repeat(32))
  assert.equal(fitRules(`a\n${'─'.repeat(90)}\nb`), `a\n${'─'.repeat(32)}\nb`)
  assert.equal(fitRules(`  ${'━'.repeat(120)}   `), `  ${'━'.repeat(32)}`)
  assert.equal(fitRules(`${'═'.repeat(60)}\r\n`), `${'═'.repeat(32)}\n`, 'CRLF 的 \\r 算行尾空白')
})

test('輸入框上下的分隔線（agent TUI）：每一條都縮、中間的 `>` 提示不動', () => {
  const rule = '─'.repeat(310)
  assert.equal(fitRules([rule, '>', rule].join('\n')), ['─'.repeat(32), '>', '─'.repeat(32)].join('\n'))
})

test('混用不同分隔線字元的整列也縮', () => {
  const mixed = '─'.repeat(30) + '━'.repeat(30) + '╌'.repeat(30)
  assert.equal(fitRules(mixed), mixed.slice(0, 32))
})

test('不是整列只有分隔線的不動：行內的線、前後有字、短線', () => {
  const long = '─'.repeat(90)
  for (const keep of [`text ${long}`, `${long} text`, `│ ${long} │`, `a${long}`, '─'.repeat(39), '│ rm -rf x ──── y', '', 'plain text']) {
    assert.equal(fitRules(keep), keep)
  }
})

test('已經夠短的整列（40 個以下）不動；40 個以上才縮', () => {
  assert.equal(fitRules('─'.repeat(40)), '─'.repeat(32))
  assert.equal(fitRules('─'.repeat(32)), '─'.repeat(32))
})
