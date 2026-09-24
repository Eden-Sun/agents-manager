import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message } from '../api/types.ts'
import { clearHookCompletion, completionKey, countUnreadTurns, idleEdgeCompletionKey, isUnread, loadCounts, loadMarks, markHookCompletion, markOfMessages, projectUnread, resetIdleEdges, resetTurnCompletions, saveCounts, saveMarks, takeTurnCompletion, titleUnread, totalUnread, unreadShown } from './unread.ts'

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

/** 防重複計數：沒 turn_id 的回覆兩種 frame 的 key 曾對不上，一則回覆跳兩下。 */
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

test('分頁標題不算總管專案的未讀：側欄不顯示的，標題也不能掛著點不掉的 (N)（review3 c5 L2）', () => {
  const bots = [
    { id: 'agm', project_id: 'p-agm' },
    { id: 'agm-kid', project_id: 'p-agm' },
    { id: 'work', project_id: 'p1' },
    { id: 'folded', project_id: 'p1' },
  ]
  const s = { bots, botUnread: { agm: 40, 'agm-kid': 3, work: 2, folded: 1 }, hiddenBotIds: ['folded'], supervisorProjectId: 'p-agm' }
  assert.equal(titleUnread(s), 2)
  assert.equal(unreadShown(bots[0], 'p-agm'), false)
  assert.equal(unreadShown(bots[2], 'p-agm'), true)
  // 還不知道總管專案是哪個（`GET /api/supervisor` 讀不到）：不排除。
  assert.equal(titleUnread({ ...s, supervisorProjectId: null }), 45)
  assert.equal(unreadShown(undefined, 'p-agm'), true)
})

test('專案收合的 !N 與分頁標題同一份排除：額度隱藏與總管專案的都不算（issue #510）', () => {
  const bots = [
    { id: 'work', project_id: 'p1' },
    { id: 'quota-hidden', project_id: 'p1' },
    { id: 'other', project_id: 'p2' },
    { id: 'agm', project_id: 'p-agm' },
  ]
  const s = {
    bots,
    botUnread: { work: 2, 'quota-hidden': 3, other: 5, agm: 40 },
    hiddenBotIds: ['quota-hidden'],
    supervisorProjectId: 'p-agm',
  }
  // 側欄收合 p1 時掛的數字：只有展開後點得到的那一筆。
  assert.equal(projectUnread(s, 'p1'), 2)
  assert.equal(projectUnread(s, 'p2'), 5)
  // 總管專案的收合加總也是 0（側欄本來就不顯示它們的未讀）。
  assert.equal(projectUnread(s, 'p-agm'), 0)
  // 兩邊同一份排除：分頁標題與各專案加總對得起來。
  assert.equal(titleUnread(s), projectUnread(s, 'p1') + projectUnread(s, 'p2'))
  // 額度停用解除後那三筆就回來了。
  assert.equal(projectUnread({ ...s, hiddenBotIds: [] }, 'p1'), 5)
  // 剛刪掉、`bots` 裡已經沒有但帳還沒 prune 的：`hiddenBotIds` 照原樣帶進排除名單才擋得掉。
  assert.equal(titleUnread({ ...s, botUnread: { ...s.botUnread, gone: 7 }, hiddenBotIds: ['quota-hidden', 'gone'] }), 2 + 5)
})
