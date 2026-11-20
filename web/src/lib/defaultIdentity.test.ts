import { test } from 'node:test'
import assert from 'node:assert/strict'
import { canonicalIdentity, defaultIdentityName, isBuiltinDefaultIdentity } from './defaultIdentity.ts'
import { runtimeDrift } from './runtimeDrift.ts'
import { quotaBaseKey } from '../store/quotaLookup.ts'
import type { Bot, Identity, Run } from '../api/types.ts'

test('預設帳號的名字：claude＝cc0、agy＝ag0，其他 kind 沒有固定名字', () => {
  assert.equal(defaultIdentityName('claude'), 'cc0')
  assert.equal(defaultIdentityName('agy'), 'ag0')
  assert.equal(defaultIdentityName('codex'), null)
})

test('agy 的 ag0／舊資料 agy／空值是同一個預設帳號；ag1 與別的 kind 不受影響', () => {
  assert.equal(isBuiltinDefaultIdentity('agy', 'ag0'), true)
  assert.equal(isBuiltinDefaultIdentity('agy', 'agy'), true)
  assert.equal(isBuiltinDefaultIdentity('agy', 'ag1'), false)
  assert.equal(isBuiltinDefaultIdentity('claude', 'ag0'), false)
  assert.equal(canonicalIdentity('agy', 'ag0'), null)
  assert.equal(canonicalIdentity('agy', ' agy '), null)
  assert.equal(canonicalIdentity('agy', ''), null)
  assert.equal(canonicalIdentity('agy', 'ag1'), 'ag1')
  assert.equal(canonicalIdentity('claude', 'cc0'), 'cc0', 'cc0 是真的 shell 身份，不收斂')
})

test('ag0 的額度落在裸 agy key（它不在身份表裡），ag1 落在自己那一格', () => {
  const ag1: Identity = { name: 'ag1', kind: 'agy', env: { HOME: '$HOME/.agy-ag1' }, args: [] }
  assert.equal(quotaBaseKey({}, 'local', 'agy', 'ag0', [ag1]), 'agy')
  assert.equal(quotaBaseKey({}, 'local', 'agy', 'agy', [ag1]), 'agy')
  assert.equal(quotaBaseKey({}, 'local', 'agy', null, [ag1]), 'agy')
  assert.equal(quotaBaseKey({}, 'local', 'agy', 'ag1', [ag1]), 'agy:ag1')
})

test('設定沒指定、實際跑在 ag0（或反過來）不算帳號不一致；ag0 對 ag1 才算，標籤寫 ag0', () => {
  const bot = (identity: string | null) => ({ id: 'b', kind: 'agy', model: null, effort: null, fast: false, identity }) as unknown as Bot
  const run = (runtime_identity: string) =>
    ({ id: 'r', bot_id: 'b', state: 'running', runtime_identity, runtime_model: null, runtime_effort: null, runtime_fast: null }) as unknown as Run
  assert.deepEqual(runtimeDrift(bot(null), run('ag0')), [])
  assert.deepEqual(runtimeDrift(bot('ag0'), run('')), [])
  const d = runtimeDrift(bot('ag1'), run(''))
  assert.equal(d.length, 1)
  assert.equal(d[0].running, 'ag0')
  assert.equal(d[0].configured, 'ag1')
})
