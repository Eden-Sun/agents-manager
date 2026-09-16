import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot } from '../api/types.ts'
import { laterMark, serverUnread } from './sharedUnread.ts'

const bot = (id: string, unread?: number, extra: Partial<Bot> = {}) => ({ id, unread, ...extra }) as Bot

test('手機分頁睡著沒收到回合完成：daemon 的數字補上；在別台讀過的清掉', () => {
  assert.deepEqual(serverUnread([bot('a', 2), bot('b', 0), bot('c', 1)], { b: 3 }, () => false), { a: 2, c: 1 })
})

test('正在看的那顆、佔位列、舊 daemon 沒給數字的都不動；沒變就回 null', () => {
  assert.equal(serverUnread([bot('a', 5), bot('p', 1, { pending: true }), bot('old')], { a: 1, old: 4 }, (id) => id === 'a'), null)
  assert.equal(serverUnread([bot('a', 1)], { a: 1 }, () => false), null)
})

test('本機與 daemon 的標記取較新的；同時間戳看 id', () => {
  const local = { at: '2026-09-15T01:00:00.000Z', id: 'm1' }
  const server = { at: '2026-09-15T02:00:00.000Z', id: 'm2' }
  assert.equal(laterMark(local, server), server)
  assert.equal(laterMark(server, local), server)
  assert.equal(laterMark(undefined, server), server)
  assert.equal(laterMark(local, null), local)
  assert.equal(laterMark({ at: server.at, id: 'm3' }, server)?.id, 'm3')
})

test('已讀還沒送到 daemon 的那顆跳過：快照的舊數字不可以把徽章點回來', () => {
  const unsent = new Set(['a'])
  assert.equal(serverUnread([bot('a', 3)], {}, () => false, unsent), null)
  assert.deepEqual(serverUnread([bot('a', 3), bot('b', 2)], {}, () => false, unsent), { b: 2 })
})
