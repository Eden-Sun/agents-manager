import test from 'node:test'
import assert from 'node:assert/strict'
import { GPT_61_SOL_CONTEXT_WINDOW, effectiveModelOf, isGpt61Sol, sol500kContext, solWindowApplies } from './codexContextWindow'

test('只認 gpt-6.1-sol（不分大小寫）；其他 Sol 版本、Luna、Astra、claude 都不算', () => {
  assert.equal(isGpt61Sol('gpt-6.1-sol'), true)
  assert.equal(isGpt61Sol('GPT-6.1-Sol'), true)
  assert.equal(isGpt61Sol(' gpt-6.1-sol '), true)
  for (const m of ['gpt-6-sol', 'gpt-5.6-sol', 'gpt-6.1-luna', 'gpt-6-astra', 'opus', 'claude-opus-5-5', '', null, undefined]) {
    assert.equal(isGpt61Sol(m), false, String(m))
  }
})

test('適用條件：kind 必須是 codex 才套 Sol 規則；claude 即使模型名一樣也不套', () => {
  assert.equal(solWindowApplies('codex', 'gpt-6.1-sol'), true)
  assert.equal(solWindowApplies('claude', 'gpt-6.1-sol'), false)
  assert.equal(solWindowApplies('grok', 'gpt-6.1-sol'), false)
  assert.equal(solWindowApplies(null, 'gpt-6.1-sol'), false)
  assert.equal(sol500kContext('claude', 'gpt-6.1-sol', 73_000), null)
})

test('有效模型：runtime_model 去空白後非空才用它，空字串／空白回退到設定值', () => {
  assert.equal(effectiveModelOf('gpt-6.1-sol', 'gpt-6-luna'), 'gpt-6.1-sol')
  assert.equal(effectiveModelOf('', 'gpt-6.1-sol'), 'gpt-6.1-sol')
  assert.equal(effectiveModelOf('   ', 'gpt-6.1-sol'), 'gpt-6.1-sol')
  assert.equal(effectiveModelOf(null, 'gpt-6.1-sol'), 'gpt-6.1-sol')
  assert.equal(effectiveModelOf('', ''), null)
  assert.equal(effectiveModelOf(undefined, undefined), null)
})

test('gpt-6.1-sol：73000 token → 14.6%（分母 500k）', () => {
  assert.equal(GPT_61_SOL_CONTEXT_WINDOW, 500_000)
  const v = sol500kContext('codex', 'gpt-6.1-sol', 73_000)!
  assert.equal(Math.round(v.pct * 10) / 10, 14.6)
  assert.equal(v.tokens, 73_000)
})

test('gpt-6.1-sol：0 token 合法（0%）；NaN、Infinity、負數、缺值都不算', () => {
  assert.deepEqual(sol500kContext('codex', 'gpt-6.1-sol', 0), { pct: 0, tokens: 0 })
  for (const bad of [null, undefined, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1]) {
    assert.equal(sol500kContext('codex', 'gpt-6.1-sol', bad), null, String(bad))
  }
})

test('非 gpt-6.1-sol 一律回 null（既有規則照舊）', () => {
  assert.equal(sol500kContext('codex', 'gpt-6-sol', 73_000), null)
  assert.equal(sol500kContext('codex', 'gpt-6-luna', 73_000), null)
  assert.equal(sol500kContext('codex', null, 73_000), null)
})
