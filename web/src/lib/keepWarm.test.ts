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

test('keepWarmReplied 純函式：保溫後 TTL 內有框、過 TTL 沒框、送 prompt 立即沒框', () => {
  const t0 = Date.parse('2026-10-05T10:00:00.000Z')
  const at = (minAgo: number) => new Date(t0 - minAgo * 60_000).toISOString()

  // 1) 保溫後 TTL 內有框：58 分保溫、現在在 70 分（保溫後 12 分鐘，TTL 3600 秒內）
  const warmRun = {
    keep_warm_replied_at: at(12),
    cache_kept_warm_at: at(12),
    last_api_at: at(70),
    cache_ttl_secs: 3600,
  }
  assert.equal(keepWarmReplied(warmRun, t0), true, '保溫後 TTL 內有框')

  // 2) 過 TTL 沒框：保溫已過了 61 分鐘（已涼）
  const coldRun = {
    keep_warm_replied_at: at(61),
    cache_kept_warm_at: at(61),
    last_api_at: at(120),
    cache_ttl_secs: 3600,
  }
  assert.equal(keepWarmReplied(coldRun, t0), false, '過 TTL 沒框')

  // 3) 送 prompt 立即沒框：使用者送 prompt 後 daemon 把 keep_warm_replied_at 清為 null
  const promptedRun = {
    ...warmRun,
    keep_warm_replied_at: null,
  }
  assert.equal(keepWarmReplied(promptedRun, t0), false, '送 prompt 立即沒框')

  // 4) 邊界與保護：無 run、空物件、grok（無 TTL）
  assert.equal(keepWarmReplied(null, t0), false)
  assert.equal(keepWarmReplied({}, t0), false)
  assert.equal(keepWarmReplied({ keep_warm_replied_at: at(12), cache_ttl_secs: null }, t0), false, 'grok 無 TTL 不畫框')
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
