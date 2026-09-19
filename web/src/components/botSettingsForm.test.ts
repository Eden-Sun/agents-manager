import test from 'node:test'
import assert from 'node:assert/strict'
import { computeBotPatch, effectiveForm, INSTRUCTION_FILES_CHOICES, type BotFormBase, type BotFormKey, type BotFormValues } from './botSettingsForm.ts'
import { INSTRUCTION_FILES } from '../api/types.ts'

const base: BotFormBase = { name: 'am', model: 'opus', effort: 'high', fast: false, persona: null, identity: null, instruction_files: 'claude-md' }
const same: BotFormValues = { name: 'am', model: 'opus', effort: 'high', fast: false, persona: '', identity: '', instruction_files: 'claude-md' }
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

test('專案指示檔：選了別的值才送；改回原值、沒動過都不送', () => {
  const shared = { ...same, instruction_files: 'claude-md-and-agents-md' as const }
  assert.deepEqual(computeBotPatch(base, shared, t('instruction_files'), 'claude'), { instruction_files: 'claude-md-and-agents-md' })
  assert.deepEqual(computeBotPatch(base, shared, t(), 'claude'), {}, '沒動過就不送')
  assert.deepEqual(computeBotPatch(base, same, t('instruction_files'), 'claude'), {}, '動過但跟 base 相同')
})

test('專案指示檔：面板開著時別處改了，沒動過的跟著 store，不會拿開啟當下的舊值蓋回去', () => {
  const updated: BotFormBase = { ...base, instruction_files: 'managed-only' }
  assert.deepEqual(computeBotPatch(updated, same, t(), 'claude'), {})
  assert.equal(effectiveForm(updated, same, t()).instruction_files, 'managed-only')
  // 使用者自己選了就以使用者的為準。
  assert.deepEqual(computeBotPatch(updated, { ...same, instruction_files: 'claude-md-or-agents-md' }, t('instruction_files'), 'claude'), {
    instruction_files: 'claude-md-or-agents-md',
  })
})

test('專案指示檔：只有 claude、而且 daemon 有給這一格（base 不是 null）才送', () => {
  const shared = { ...same, instruction_files: 'claude-md-and-agents-md' as const }
  for (const kind of ['codex', 'grok'] as const) assert.deepEqual(computeBotPatch(base, shared, t('instruction_files'), kind), {}, kind)
  assert.deepEqual(computeBotPatch({ ...base, instruction_files: null }, shared, t('instruction_files'), 'claude'), {}, '舊 daemon 沒有這一格')
})

test('專案指示檔：面板的選項就是 API 的四個值，順序一致，預設在最前面', () => {
  assert.deepEqual(INSTRUCTION_FILES_CHOICES.map((c) => c.value), [...INSTRUCTION_FILES])
  assert.equal(INSTRUCTION_FILES_CHOICES[0].value, 'claude-md')
  for (const c of INSTRUCTION_FILES_CHOICES) assert.ok(c.label && c.hint, c.value)
})
