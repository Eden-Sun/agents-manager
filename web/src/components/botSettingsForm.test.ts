import test from 'node:test'
import assert from 'node:assert/strict'
import { computeBotPatch, effectiveForm, type BotFormBase, type BotFormKey, type BotFormValues } from './botSettingsForm.ts'

const base: BotFormBase = { name: 'am', model: 'opus', effort: 'high', fast: false, persona: null, identity: null }
const same: BotFormValues = { name: 'am', model: 'opus', effort: 'high', fast: false, persona: '', identity: '' }
const t = (...k: BotFormKey[]) => new Set<BotFormKey>(k)

test('什麼都沒動：空 patch，不算 dirty', () => {
  assert.deepEqual(computeBotPatch(base, same, t(), 'claude'), {})
})

test('動過但值相等（改回去）也不送', () => {
  assert.deepEqual(computeBotPatch(base, same, t('name', 'model', 'effort'), 'claude'), {})
})

test('面板開著時別處把 effort 改成 max：沒動過的欄位跟著 store，不會拿舊值蓋回去', () => {
  const updated = { ...base, effort: 'max' }
  // 表單 state 還停在開啟當下的 high。
  const stale = { ...same, effort: 'high' }
  assert.deepEqual(computeBotPatch(updated, stale, t(), 'claude'), {})
  assert.equal(effectiveForm(updated, stale, t()).effort, 'max')
})

test('使用者自己選了 effort，別處再改也以使用者的為準', () => {
  const updated = { ...base, effort: 'max' }
  const mine = { ...same, effort: 'low' }
  assert.deepEqual(computeBotPatch(updated, mine, t('effort'), 'claude'), { effort: 'low' })
})

test('fast 只有 codex 會送', () => {
  const f = { ...same, fast: true }
  assert.deepEqual(computeBotPatch(base, f, t('fast'), 'claude'), {})
  assert.deepEqual(computeBotPatch(base, f, t('fast'), 'codex'), { fast: true })
})

test('identity 三種 kind 都送，空字串 = 不指定（null）', () => {
  const withId = { ...base, identity: 'cc2' }
  for (const kind of ['claude', 'codex', 'grok'] as const) {
    assert.deepEqual(computeBotPatch(base, { ...same, identity: 'cc2' }, t('identity'), kind), { identity: 'cc2' })
    assert.deepEqual(computeBotPatch(withId, { ...same, identity: '' }, t('identity'), kind), { identity: null })
  }
})

test('persona 空白 → null；跟 base 的 null 相等就不送', () => {
  assert.deepEqual(computeBotPatch(base, { ...same, persona: '   ' }, t('persona'), 'claude'), {})
  assert.deepEqual(computeBotPatch(base, { ...same, persona: ' 你是 reviewer ' }, t('persona'), 'claude'), { persona: '你是 reviewer' })
  assert.deepEqual(computeBotPatch({ ...base, persona: 'x' }, { ...same, persona: '' }, t('persona'), 'claude'), { persona: null })
})
