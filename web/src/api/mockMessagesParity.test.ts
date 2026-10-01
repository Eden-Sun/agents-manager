/**
 * `GET /api/bots/{id}/messages` 要跟 `daemon/src/api.rs::get_messages` 一樣：不存在的 bot 是 404（已軟刪的仍讀得到歷史，
 * API.md §10.4），`limit` 夾在 1..500（`0` 變 1，不是變 100）。以前 mock 對不存在的 id 回一頁空的，
 * 前端對這個 404 的處理在 `VITE_MOCK=1` 底下看不到。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

const firstBotId = async (m: MockTransport) =>
  ((await m.request('GET', '/state')) as { projects: { bots: { id: string }[] }[] }).projects.flatMap((p) => p.bots)[0].id

test('不存在的 bot：404 not_found（what: bot）', async () => {
  const m = new MockTransport()
  await assert.rejects(m.request('GET', '/bots/no-such-bot/messages'), (e: unknown) => {
    assert.ok(e instanceof ApiError)
    assert.equal(e.status, 404)
    assert.equal(e.body.error, 'not_found')
    assert.equal(e.body.what, 'bot')
    return true
  })
})

test('已軟刪的 bot 仍讀得到歷史（不是 404）', async () => {
  const m = new MockTransport()
  const id = await firstBotId(m)
  const before = (await m.request('GET', `/bots/${id}/messages`)) as { messages: unknown[] }
  await m.request('DELETE', `/bots/${id}`)
  const after = (await m.request('GET', `/bots/${id}/messages`)) as { messages: unknown[] }
  assert.equal(after.messages.length, before.messages.length)
})

test('limit 夾在 1..500：0 變 1、不是數字變預設 100', async () => {
  const m = new MockTransport()
  const id = await firstBotId(m)
  const n = async (q: string) => ((await m.request('GET', `/bots/${id}/messages?${q}`)) as { messages: unknown[] }).messages.length
  assert.equal(await n('limit=0'), 1)
  assert.equal(await n('limit=-5'), 1)
  assert.equal(await n('limit=2'), 2)
  assert.ok((await n('limit=abc')) <= 100)
})
