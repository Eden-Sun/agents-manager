import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message } from '../api/types.ts'
import { isKeepWarmRequestId, keepWarmReplied, keepWarmSkippable } from './keepWarm.ts'
import { completesTurn, countUnreadTurns } from '../store/unread.ts'
import { toMessage, toRun } from '../api/normalize.ts'

test('保溫回合的 client_request_id：新前綴與 DB 舊前綴都認', () => {
  assert.equal(isKeepWarmRequestId('keep-warm:01ABC'), true)
  assert.equal(isKeepWarmRequestId('keepalive:01ABC'), true)
  assert.equal(isKeepWarmRequestId('web-1'), false)
  assert.equal(isKeepWarmRequestId(null), false)
  assert.equal(isKeepWarmRequestId(''), false)
})

test('「不用保溫」只給主力的 claude／codex', () => {
  assert.equal(keepWarmSkippable({ primary: true, kind: 'claude' }), true)
  assert.equal(keepWarmSkippable({ primary: true, kind: 'codex' }), true)
  assert.equal(keepWarmSkippable({ primary: true, kind: 'grok' }), false)
  assert.equal(keepWarmSkippable({ primary: false, kind: 'claude' }), false)
  assert.equal(keepWarmSkippable(undefined), false)
})

test('keep_warm_replied_at 非 null 才算保溫回覆到了', () => {
  assert.equal(keepWarmReplied({ keep_warm_replied_at: '2026-10-05T10:00:00Z' }), true)
  assert.equal(keepWarmReplied({ keep_warm_replied_at: null }), false)
  assert.equal(keepWarmReplied({}), false)
  assert.equal(keepWarmReplied(null), false)
})

const raw = (keepWarm?: boolean) => ({
  id: 'm1', conversation_id: 'c', turn_id: 't1', role: 'assistant', content: 'x', source: 'hook', created_at: '2026-10-05T10:00:00Z',
  ...(keepWarm === undefined ? {} : { keep_warm: keepWarm }),
})

test('訊息的 keep_warm 進得了 normalize，保溫回覆不算完成回合、不算未讀', () => {
  const kw = toMessage(raw(true))!
  const normal = toMessage(raw())!
  assert.equal(kw.keep_warm, true)
  assert.equal(normal.keep_warm, undefined)
  assert.equal(completesTurn(kw), false)
  assert.equal(completesTurn(normal), true)
  assert.equal(countUnreadTurns([kw as Message], undefined), 0)
  assert.equal(countUnreadTurns([kw as Message, { ...(normal as Message), id: 'm2', turn_id: 't2' }], undefined), 1)
})

test('run：新欄位 cache_kept_warm_at／keep_warm_skip／keep_warm_replied_at 進得了 normalize，缺欄位給安全預設', () => {
  const r = toRun({ id: 'r1', bot_id: 'b1', state: 'running', cache_kept_warm_at: '2026-10-05T10:00:00Z', keep_warm_skip: true, keep_warm_replied_at: '2026-10-05T10:01:00Z' }, 'b1')!
  assert.equal(r.cache_kept_warm_at, '2026-10-05T10:00:00Z')
  assert.equal(r.keep_warm_skip, true)
  assert.equal(r.keep_warm_replied_at, '2026-10-05T10:01:00Z')
  const old = toRun({ id: 'r1', bot_id: 'b1', state: 'running' }, 'b1')!
  assert.equal(old.keep_warm_skip, false)
  assert.equal(old.keep_warm_replied_at, null)
  assert.equal(old.cache_kept_warm_at, null)
})
