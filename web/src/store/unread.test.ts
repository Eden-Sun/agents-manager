import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message } from '../api/types.ts'
import { clearHookCompletion, completionKey, countUnreadTurns, idleEdgeCompletionKey, isUnread, loadCounts, loadMarks, markHookCompletion, markOfMessages, resetIdleEdges, resetTurnCompletions, saveCounts, saveMarks, takeTurnCompletion, totalUnread } from './unread.ts'

/** node 沒有 localStorage；這裡只要 get/set 兩支。 */
function stubStorage() {
  const map = new Map<string, string>()
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
  }
  return map
}

function msg(id: string, role: Message['role'], created_at: string, turn_id: string | null = null): Message {
  return {
    id,
    conversation_id: 'c1',
    turn_id,
    role,
    content: id,
    source: 'hook',
    incomplete: false,
    group_id: null,
    attachments: [],
    team_id: null,
    relay_from: null,
    terminal_snapshot: null,
    created_at,
  }
}

test('沒有標記 = 全部未讀，但只算 assistant', () => {
  const ms = [msg('1', 'user', '2026-09-07T00:00:01Z'), msg('2', 'assistant', '2026-09-07T00:00:02Z', 't1')]
  assert.equal(countUnreadTurns(ms, undefined), 1)
})

test('同一個回合的多則回覆只算一個未讀', () => {
  const ms = [
    msg('1', 'assistant', '2026-09-07T00:00:01Z', 't1'),
    msg('2', 'assistant', '2026-09-07T00:00:02Z', 't1'),
    msg('3', 'assistant', '2026-09-07T00:00:03Z', 't2'),
  ]
  assert.equal(countUnreadTurns(ms, undefined), 2)
})

test('沒有 turn_id 的回覆各自算一個回合', () => {
  const ms = [msg('1', 'assistant', '2026-09-07T00:00:01Z'), msg('2', 'assistant', '2026-09-07T00:00:02Z')]
  assert.equal(countUnreadTurns(ms, undefined), 2)
})

test('標記之後的才算未讀；標記那一則自己已讀', () => {
  const ms = [
    msg('1', 'assistant', '2026-09-07T00:00:01Z', 't1'),
    msg('2', 'assistant', '2026-09-07T00:00:02Z', 't2'),
    msg('3', 'assistant', '2026-09-07T00:00:03Z', 't3'),
  ]
  assert.equal(countUnreadTurns(ms, { at: '2026-09-07T00:00:02Z', id: '2' }), 1)
})

test('時間戳撞在一起時靠 id 分辨標記那一則', () => {
  const same = '2026-09-07T00:00:02Z'
  assert.equal(isUnread({ id: 'a', created_at: same }, { at: same, id: 'a' }), false)
  assert.equal(isUnread({ id: 'b', created_at: same }, { at: same, id: 'a' }), true)
})

test('markOfMessages 取時間最大的那一則（清單沒排序也一樣）', () => {
  const ms = [msg('2', 'assistant', '2026-09-07T00:00:09Z'), msg('1', 'user', '2026-09-07T00:00:01Z')]
  assert.deepEqual(markOfMessages(ms), { at: '2026-09-07T00:00:09Z', id: '2' })
  assert.equal(markOfMessages([]), null)
})

test('同一個回合的 message_added 與 turn_updated 只跳一次', () => {
  resetTurnCompletions()
  assert.equal(takeTurnCompletion('b1', 't1'), true)
  assert.equal(takeTurnCompletion('b1', 't1'), false)
  // 不同 bot 的同名 turn 是不同回合。
  assert.equal(takeTurnCompletion('b2', 't1'), true)
})

test('totalUnread 只加 bot——群組未讀是同一批回覆的第二份帳', () => {
  assert.equal(totalUnread({ a: 2, b: 1 }), 3)
})

test('未讀數與已讀標記存得回來（跨重整的那一段）', () => {
  stubStorage()
  saveCounts({ bots: { b1: 2 }, groups: { p1: 1 } })
  saveMarks({ 'bot:b1': { at: '2026-09-07T00:00:02Z', id: 'm2' } })
  assert.deepEqual(loadCounts(), { bots: { b1: 2 }, groups: { p1: 1 } })
  assert.deepEqual(loadMarks(), { 'bot:b1': { at: '2026-09-07T00:00:02Z', id: 'm2' } })
})

test('0 不寫進去；讀不到 / 壞掉的內容當作全部已讀', () => {
  const map = stubStorage()
  saveCounts({ bots: { b1: 0 }, groups: {} })
  assert.equal(map.get('am.unread'), '{}')
  map.set('am.unread', 'not json')
  map.set('am.readMarks', '[1,2]')
  assert.deepEqual(loadCounts(), { bots: {}, groups: {} })
  assert.deepEqual(loadMarks(), {})
})

test('localStorage 整支不能用時不會炸（無痕視窗）', () => {
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: () => { throw new Error('denied') },
    setItem: () => { throw new Error('denied') },
  }
  assert.deepEqual(loadCounts(), { bots: {}, groups: {} })
  assert.deepEqual(loadMarks(), {})
  saveCounts({ bots: { b1: 1 }, groups: {} })
  saveMarks({ 'bot:b1': { at: 'x', id: 'y' } })
})

// ---------------------------------------------------------- 一個回合只跳一下

test('有 turn_id 就用 turn_id 記帳', () => {
  assert.equal(completionKey(msg('m1', 'assistant', '2026-09-07T00:00:01Z', 't1'), ['t1']), 't1')
})

test('沒有 turn_id 時掛在最近的回合上，跟 turn_updated 同一個 key', () => {
  const m = msg('m1', 'assistant', '2026-09-07T00:00:01Z')
  assert.equal(completionKey(m, ['t1', 't3', 't2']), 't3')
})

test('連一個回合都不知道時才退回 msg:<id>', () => {
  assert.equal(completionKey(msg('m1', 'assistant', '2026-09-07T00:00:01Z'), []), 'msg:m1')
})

/**
 * 這就是重複計數的那個 bug：沒有 turn_id 的回覆先被 `message_added` 記一次、`turn_updated`
 * 再記一次，`takeTurnCompletion` 兩個 key 對不上，一則回覆讓徽章跳兩下。兩種 frame 順序都要
 * 只跳一次。
 */
test('message_added 與 turn_updated 不管誰先到，同一個回合只記一次', () => {
  for (const messageFirst of [true, false]) {
    resetTurnCompletions()
    const m = msg('m1', 'assistant', '2026-09-07T00:00:01Z')
    // `turn_updated`（in_flight）已經讓 store 認得這個回合，兩條路都看得到它。
    const known = ['t1']
    const hits = messageFirst
      ? [takeTurnCompletion('b1', completionKey(m, known)), takeTurnCompletion('b1', 't1')]
      : [takeTurnCompletion('b1', 't1'), takeTurnCompletion('b1', completionKey(m, known))]
    assert.deepEqual(hits.filter(Boolean).length, 1, `messageFirst=${messageFirst}`)
  }
})

test('idle 邊緣：最近的回合還在飛就共用它的 turn id，之後 turn_updated 到了會被去重', () => {
  resetIdleEdges()
  resetTurnCompletions()
  const key = idleEdgeCompletionKey('b1', 'r1', { id: 't9', status: 'in_flight' })
  assert.equal(key, 't9')
  assert.equal(takeTurnCompletion('b1', key!), true)
  assert.equal(takeTurnCompletion('b1', 't9'), false)
})

test('idle 邊緣：hook 那條路已先記過（turn_updated 先到），這次 idle 是同一回合的尾巴，跳過', () => {
  resetIdleEdges()
  markHookCompletion('b1')
  assert.equal(idleEdgeCompletionKey('b1', 'r1', { id: 't9', status: 'completed' }), null)
  // 標記只用一次。
  assert.equal(idleEdgeCompletionKey('b1', 'r1', { id: 't9', status: 'completed' }), 'run:r1:1')
})

test('idle 邊緣：已有 web 回合但這次是終端直接輸入 → 獨立 key，每次都算一則新的', () => {
  resetIdleEdges()
  resetTurnCompletions()
  // 之前從網頁送過的回合已經記過 t9。
  takeTurnCompletion('b1', 't9')
  const k1 = idleEdgeCompletionKey('b1', 'r1', { id: 't9', status: 'completed' })
  const k2 = idleEdgeCompletionKey('b1', 'r1', { id: 't9', status: 'completed' })
  assert.deepEqual([k1, k2], ['run:r1:1', 'run:r1:2'])
  assert.equal(takeTurnCompletion('b1', k1!), true)
  assert.equal(takeTurnCompletion('b1', k2!), true)
})

test('idle 邊緣：新回合開始會清掉上一輪的 hook 標記', () => {
  resetIdleEdges()
  markHookCompletion('b1')
  clearHookCompletion('b1')
  assert.equal(idleEdgeCompletionKey('b1', 'r1', null), 'run:r1:1')
})
