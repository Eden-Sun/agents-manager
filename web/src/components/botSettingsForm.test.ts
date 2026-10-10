import test from 'node:test'
import assert from 'node:assert/strict'
import { computeBotPatch, effectiveForm, pruneSaved, settledKeys, type BotFormBase, type BotFormKey, type BotFormValues } from './botSettingsForm.ts'

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

test('面板開著時別處改了，沒動過的跟著 store，不會拿開啟當下的舊值蓋回去', () => {
  const updated: BotFormBase = { ...base, persona: '別處改的人設' }
  assert.deepEqual(computeBotPatch(updated, same, t(), 'claude'), {})
  assert.equal(effectiveForm(updated, same, t()).persona, '別處改的人設')
  // 使用者自己改了就以使用者的為準。
  assert.deepEqual(computeBotPatch(updated, { ...same, persona: '我的人設' }, t('persona'), 'claude'), { persona: '我的人設' })
})

test('store 追上存過的值：那一欄從 saved 拿掉，之後別處改了才不會被舊的存值蓋住', () => {
  const saved = { model: 'sonnet', effort: 'low' }
  assert.deepEqual(pruneSaved(saved, { ...base, model: 'sonnet', effort: 'high' }), { effort: 'low' })
})

test('store 還沒追上：存值照留（避免誤跳放棄未儲存）', () => {
  const saved = { model: 'sonnet' }
  assert.equal(pruneSaved(saved, base), saved)
})


test('存過 null（清回預設）與 fast：以 store 的值相等為準', () => {
  assert.deepEqual(pruneSaved({ effort: null, fast: true }, { ...base, effort: null, fast: true }), {})
  assert.deepEqual(pruneSaved({ persona: null }, { ...base, persona: 'x' }), { persona: null })
})

test('settledKeys：送出後沒再改的欄位才放掉', () => {
  assert.deepEqual(
    settledKeys({ name: 'n1', persona: 'A' }, { name: 'n1', model: null, effort: null, fast: false, persona: 'A', identity: '' }),
    ['name', 'persona'],
  )
})

test('settledKeys：送出後又改過的那一欄留著', () => {
  assert.deepEqual(
    settledKeys({ name: 'n1', persona: 'A' }, { name: 'n1', model: null, effort: null, fast: false, persona: 'AB', identity: '' }),
    ['name'],
  )
})

test('settledKeys：正規化跟送出時一致', () => {
  const sent = { persona: null, identity: null, fast: true, model: null }
  const now = { name: 'x', model: null, effort: 'high', fast: true, persona: '   ', identity: '' }
  assert.deepEqual(settledKeys(sent, now), ['model', 'fast', 'persona', 'identity'])
  assert.deepEqual(
    settledKeys({ persona: 'A' }, { name: 'x', model: null, effort: null, fast: false, persona: ' A ', identity: '' }),
    ['persona'],
  )
})

test('settledKeys：沒送出的 key 不會出現', () => {
  assert.deepEqual(settledKeys({}, same), [])
})
