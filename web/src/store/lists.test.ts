import test from 'node:test'
import assert from 'node:assert/strict'
import { MESSAGE_CAP, byId, byTime, capList, insertSorted, pruneTurns } from './lists.ts'

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
  // newest two (t4, t5) + the in-flight one. `unknown` only blocks the composer while
  // in_flight (API.md §5), so the completed / failed ones go like any other settled turn.
  assert.deepEqual(Object.keys(out).sort(), ['t0', 't4', 't5'])
})

test('pruneTurns returns the same object when nothing needs dropping', () => {
  const map = { t0: { status: 'completed' } }
  assert.equal(pruneTurns(map, 50), map)
})
