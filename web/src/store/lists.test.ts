import test from 'node:test'
import assert from 'node:assert/strict'
import { MESSAGE_CAP, byId, byTime, capList, insertSorted, pruneTurns, reuseUnchanged, upsertSorted, keptAfterPage } from './lists.ts'

const m = (id: string, created_at = id) => ({ id, created_at })

test('insertSorted: the common case (newest at the end) appends', () => {
  const list = [m('a'), m('b')]
  assert.deepEqual(insertSorted(list, m('c'), byTime), [m('a'), m('b'), m('c')])
  // the input is not mutated
  assert.deepEqual(list, [m('a'), m('b')])
})

test('insertSorted: an out-of-order frame lands in the right slot', () => {
  const list = [m('a'), m('c'), m('d')]
  assert.deepEqual(insertSorted(list, m('b'), byTime), [m('a'), m('b'), m('c'), m('d')])
  assert.deepEqual(insertSorted(list, m('0'), byTime), [m('0'), m('a'), m('c'), m('d')])
})

test('insertSorted: a duplicate id returns null (nothing to apply)', () => {
  const list = [m('a'), m('b'), m('c')]
  assert.equal(insertSorted(list, m('c'), byTime), null)
  assert.equal(insertSorted(list, m('a'), byTime), null)
})

test('insertSorted: equal created_at breaks the tie on id, and keeps a stable order', () => {
  const list = [{ id: 'b', created_at: 't' }, { id: 'd', created_at: 't' }]
  assert.deepEqual(insertSorted(list, { id: 'c', created_at: 't' }, byTime), [
    { id: 'b', created_at: 't' },
    { id: 'c', created_at: 't' },
    { id: 'd', created_at: 't' },
  ])
})

test('insertSorted: byId orders the group timeline by ULID alone', () => {
  const list = [{ id: '01A' }, { id: '01C' }]
  assert.deepEqual(insertSorted(list, { id: '01B' }, byId), [{ id: '01A' }, { id: '01B' }, { id: '01C' }])
})

test('insertSorted result stays sorted no matter what order frames arrive in', () => {
  const ids = ['e', 'a', 'd', 'b', 'f', 'c']
  let list: { id: string; created_at: string }[] = []
  for (const id of ids) list = insertSorted(list, m(id), byTime) ?? list
  assert.deepEqual(list.map((x) => x.id), ['a', 'b', 'c', 'd', 'e', 'f'])
})

test('capList keeps the newest and reports whether anything was dropped', () => {
  const list = [1, 2, 3, 4, 5]
  assert.deepEqual(capList(list, 5), { list, trimmed: false })
  assert.deepEqual(capList(list, 3), { list: [3, 4, 5], trimmed: true })
  assert.equal(capList(list, 5).list, list, 'under the cap the array is returned as-is')
})

test('MESSAGE_CAP is the documented 500', () => {
  assert.equal(MESSAGE_CAP, 500)
})

test('pruneTurns keeps the newest `cap` turns', () => {
  const map: Record<string, { status: string }> = {}
  for (let i = 0; i < 10; i++) map[`t${i}`] = { status: 'completed' }
  const out = pruneTurns(map, 3)
  assert.deepEqual(Object.keys(out).sort(), ['t7', 't8', 't9'])
})

test('pruneTurns never drops an in-flight turn; a settled unknown-delivery one is not blocking anything', () => {
  const map: Record<string, { status: string; delivery?: string | null }> = {
    t0: { status: 'in_flight' },
    t1: { status: 'completed', delivery: 'unknown' },
    t2: { status: 'failed', delivery: 'unknown' },
    t3: { status: 'completed' },
    t4: { status: 'completed' },
    t5: { status: 'completed' },
  }
  const out = pruneTurns(map, 2)
  // newest two + in-flight; settled `unknown` turns don't block (API.md §5).
  assert.deepEqual(Object.keys(out).sort(), ['t0', 't4', 't5'])
})

test('pruneTurns returns the same object when nothing needs dropping', () => {
  const map = { t0: { status: 'completed' } }
  assert.equal(pruneTurns(map, 50), map)
})

test('upsertSorted：同 id 內容變了就原地換掉，一樣的回 null，新的照順序插', () => {
  type M = { id: string; t: number; content: string }
  const cmp = (a: M, b: M) => a.t - b.t
  const same = (a: M, b: M) => a.content === b.content
  const list: M[] = [{ id: 'a', t: 1, content: 'x' }, { id: 'b', t: 2, content: '備援' }, { id: 'c', t: 3, content: 'z' }]
  assert.deepEqual(upsertSorted(list, { id: 'b', t: 2, content: 'hook 原文' }, cmp, same)?.map((m) => m.content), ['x', 'hook 原文', 'z'])
  assert.equal(upsertSorted(list, { id: 'b', t: 2, content: '備援' }, cmp, same), null)
  assert.deepEqual(upsertSorted(list, { id: 'd', t: 4, content: 'w' }, cmp, same)?.map((m) => m.id), ['a', 'b', 'c', 'd'])
})

test('upsertSorted：同 id 時間變了（排過隊的一則送出時改成送出時間）要重新排位置', () => {
  type M = { id: string; t: number; content: string }
  const cmp = (a: M, b: M) => a.t - b.t
  const same = (a: M, b: M) => a.content === b.content && a.t === b.t
  const list: M[] = [{ id: 'notice', t: 1, content: 'n' }, { id: 'supp', t: 2, content: 's' }, { id: 'reply', t: 4, content: 'r' }]
  assert.deepEqual(upsertSorted(list, { id: 'notice', t: 3, content: 'n' }, cmp, same)?.map((m) => m.id), ['supp', 'notice', 'reply'])
})

test('keptAfterPage：同一毫秒、id 比頁內最新那則大的訊息要留（頁抓完後才 commit 的那一則，#695 同型）', () => {
  const T = '2026-10-01T10:00:00.123Z'
  const page = [
    { id: '01A', created_at: '2026-10-01T09:59:59.000Z' },
    { id: '01B', created_at: T },
  ]
  const existing = [
    ...page,
    { id: '01C', created_at: T }, // 同毫秒、頁抓完才進來的 message_added
    { id: '01D', created_at: '2026-10-01T10:00:00.200Z' }, // 更晚
    { id: '01Z0', created_at: '2026-10-01T09:00:00.000Z' }, // 頁裡沒有、比頁內最新舊：resync 要能刪過期的
  ]
  assert.deepEqual(
    keptAfterPage(existing, page, '2026-10-01T10:00:01.000Z').map((m) => m.id),
    ['01C', '01D'],
  )
  // 空頁退回 startedAt。
  assert.deepEqual(keptAfterPage(existing, [], T).map((m) => m.id), ['01D'])
})

test('reuseUnchanged: 內容一樣的沿用舊物件；全部一樣（同順序）連陣列也回舊的', () => {
  const prev = [{ id: 'a', n: 1, tags: ['x'] }, { id: 'b', n: 2, tags: [] }]
  const same = reuseUnchanged(prev, [{ id: 'a', n: 1, tags: ['x'] }, { id: 'b', n: 2, tags: [] }])
  assert.equal(same, prev)
  const edited = reuseUnchanged(prev, [{ id: 'a', n: 1, tags: ['x'] }, { id: 'b', n: 3, tags: [] }])
  assert.notEqual(edited, prev)
  assert.equal(edited[0], prev[0], '沒變的那則照舊')
  assert.notEqual(edited[1], prev[1])
  assert.equal(edited[1].n, 3)
})

test('reuseUnchanged: 巢狀欄位變了才換；新增、刪除、換順序都換陣列但沿用沒變的項目', () => {
  const prev = [{ id: 'a', att: [{ id: 'f1', size: 1 }] }, { id: 'b', att: [] }, { id: 'c', att: [] }]
  const nested = reuseUnchanged(prev, [{ id: 'a', att: [{ id: 'f1', size: 2 }] }, { id: 'b', att: [] }, { id: 'c', att: [] }])
  assert.notEqual(nested[0], prev[0])
  assert.equal(nested[1], prev[1])
  const removed = reuseUnchanged(prev, [{ id: 'a', att: [{ id: 'f1', size: 1 }] }, { id: 'c', att: [] }])
  assert.notEqual(removed, prev)
  assert.deepEqual(removed.map((x) => x.id), ['a', 'c'])
  assert.equal(removed[0], prev[0])
  assert.equal(removed[1], prev[2])
  const added = reuseUnchanged(prev, [...prev.map((x) => ({ ...x })), { id: 'd', att: [] }])
  assert.equal(added.length, 4)
  assert.equal(added[0], prev[0])
  const reordered = reuseUnchanged(prev, [{ id: 'b', att: [] }, { id: 'a', att: [{ id: 'f1', size: 1 }] }, { id: 'c', att: [] }])
  assert.notEqual(reordered, prev, '順序變了不能回舊陣列')
  assert.equal(reordered[0], prev[1])
  assert.deepEqual(reuseUnchanged([], [{ id: 'x' }]), [{ id: 'x' }])
})
