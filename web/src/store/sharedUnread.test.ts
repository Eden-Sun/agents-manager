import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Project } from '../api/types.ts'
import { laterMark, serverGroupUnread, serverUnread } from './sharedUnread.ts'

const bot = (id: string, unread?: number, extra: Partial<Bot> = {}) => ({ id, unread, ...extra }) as Bot

test('手機分頁睡著沒收到回合完成：daemon 的數字補上；在別台讀過的清掉', () => {
  assert.deepEqual(serverUnread([bot('a', 2), bot('b', 0), bot('c', 1)], { b: 3 }, () => false), { a: 2, c: 1 })
})

test('正在看的那顆、佔位列、舊 daemon 沒給數字的都不動；沒變就回 null', () => {
  assert.equal(serverUnread([bot('a', 5), bot('p', 1, { pending: true }), bot('old')], { a: 1, old: 4 }, (id) => id === 'a'), null)
  assert.equal(serverUnread([bot('a', 1)], { a: 1 }, () => false), null)
})

test('本機與 daemon 的標記取較新的；同時間戳優先用 seq', () => {
  const local = { at: '2026-09-15T01:00:00.000Z', id: 'm1' }
  const server = { at: '2026-09-15T02:00:00.000Z', id: 'm2' }
  assert.equal(laterMark(local, server), server)
  assert.equal(laterMark(server, local), server)
  assert.equal(laterMark(undefined, server), server)
  assert.equal(laterMark(local, null), local)
  assert.equal(laterMark({ at: server.at, id: 'm3' }, server)?.id, 'm3')
  assert.deepEqual(
    laterMark({ at: server.at, id: 'z-earlier', seq: 10 }, { at: server.at, id: 'a-later', seq: 11 }),
    { at: server.at, id: 'a-later', seq: 11 },
  )
  assert.deepEqual(
    laterMark({ at: server.at, id: 'same-message' }, { at: server.at, id: 'same-message', seq: 11 }),
    { at: server.at, id: 'same-message', seq: 11 },
    'when both records identify the same message, keep the server ordering metadata',
  )
})

test('已讀還沒送到 daemon 的那顆跳過：快照的舊數字不可以把徽章點回來', () => {
  const unsent = new Set(['a'])
  assert.equal(serverUnread([bot('a', 3)], {}, () => false, unsent), null)
  assert.deepEqual(serverUnread([bot('a', 3), bot('b', 2)], {}, () => false, unsent), { b: 2 })
})

const proj = (id: string, group_unread?: number) => ({ id, group_unread }) as Project

test('群組未讀以 daemon 的為準（#756）：別台讀過的清掉、沒開著時回來的補上', () => {
  assert.deepEqual(serverGroupUnread([proj('a', 2), proj('b', 0), proj('c', 1)], { b: 3 }, () => false), { a: 2, c: 1 })
})

test('群組：正在看的、舊 daemon 沒給數字的、已讀還沒送出的都不動；沒變就回 null', () => {
  assert.equal(serverGroupUnread([proj('a', 5), proj('old')], { a: 1, old: 4 }, (id) => id === 'a'), null)
  assert.equal(serverGroupUnread([proj('a', 1)], { a: 1 }, () => false), null)
  const unsent = new Set(['a'])
  assert.equal(serverGroupUnread([proj('a', 3)], {}, () => false, unsent), null)
  assert.deepEqual(serverGroupUnread([proj('a', 3), proj('b', 2)], {}, () => false, unsent), { b: 2 })
})
