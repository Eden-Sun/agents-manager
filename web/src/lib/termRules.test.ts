import test from 'node:test'
import assert from 'node:assert/strict'
import { fitRules } from './termRules.ts'

test('很長的整串框線縮短，短線與一般文字原樣', () => {
  assert.equal(fitRules('╌'.repeat(150)), '╌'.repeat(32))
  assert.equal(fitRules(`a\n${'─'.repeat(90)}\nb`), `a\n${'─'.repeat(32)}\nb`)
  assert.equal(fitRules('─'.repeat(39)), '─'.repeat(39))
  assert.equal(fitRules('│ rm -rf x ──── y'), '│ rm -rf x ──── y')
})
