import test from 'node:test'
import assert from 'node:assert/strict'
import { missionRequests } from './missionRequests.ts'

test('lost response retries the same request; confirmed sends permit new identical questions', async () => {
  let seq = 0
  const send = missionRequests(() => `id-${++seq}`)
  const seen: string[] = []
  await assert.rejects(send('question', 'm', 'hello', async (id) => {
    seen.push(id)
    throw new Error('server committed but connection dropped')
  }))
  await send('question', 'm', 'hello', async (id) => { seen.push(id); return true })
  await send('question', 'm', 'hello', async (id) => { seen.push(id); return true })
  assert.deepEqual(seen, ['id-1', 'id-1', 'id-2'])
})

test('concurrent submissions share the request and result', async () => {
  const send = missionRequests(() => 'same')
  let calls = 0
  let complete!: (v: string) => void
  const request = async () => { calls++; return new Promise<string>((resolve) => { complete = resolve }) }
  const a = send('revise', 'm', 'change title', request)
  const b = send('revise', 'm', 'change title', request)
  await Promise.resolve()
  assert.equal(calls, 1)
  complete('child-id')
  assert.deepEqual(await Promise.all([a, b]), ['child-id', 'child-id'])
})

test('edited text, other missions, and different operations use different keys', async () => {
  let seq = 0
  const send = missionRequests(() => `id-${++seq}`)
  const ids: string[] = []
  const fail = async (id: string) => { ids.push(id); throw new Error('network') }
  for (const [op, mission, text] of [
    ['ask', 'm', 'a'], ['ask', 'm', 'b'], ['revise', 'm', 'a'], ['ask', 'other', 'a'], ['ask', 'm', 'a'],
  ]) await assert.rejects(send(op, mission, text, fail))
  assert.deepEqual(ids, ['id-1', 'id-2', 'id-3', 'id-4', 'id-1'])
})
