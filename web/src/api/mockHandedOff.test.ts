/**
 * 專案移交給另一台主機（#708）：daemon 對那個專案的 bot 一律 409 `handed_off`（`daemon/src/handoff.rs::refuse`，
 * 開／關／重啟、送 prompt、pane RPC 都擋）。mock 以前什麼都不擋，前端沒停用的路徑在 `VITE_MOCK=1` 底下照樣成功，
 * 真環境才跳 409。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

interface BotRow {
  id: string
  project_id: string
  parent_bot_id: string | null
  run: { id: string; state: string } | null
}
const bots = async (m: MockTransport) =>
  ((await m.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)

async function refused(p: Promise<unknown>): Promise<ApiError> {
  try {
    await p
  } catch (e) {
    if (e instanceof ApiError) return e
    throw e
  }
  assert.fail('預期 409 handed_off，但請求成功了')
}

const ACTIONS: [string, string, unknown][] = [
  ['POST', 'start', undefined],
  ['POST', 'stop', undefined],
  ['POST', 'restart', undefined],
  ['POST', 'prompt', { text: 'hi', client_request_id: 'c-1' }],
  ['POST', 'keys', { keys: ['enter'] }],
  ['POST', 'text', { text: 'x' }],
  ['POST', 'interrupt', undefined],
  ['POST', 'login', undefined],
]

test('已移交的專案：bot 的 start／stop／restart／prompt／keys／text／interrupt／login 全部 409 handed_off', async () => {
  const m = new MockTransport()
  const bot = (await bots(m)).find((b) => !b.parent_bot_id)!
  await m.request('PATCH', `/projects/${bot.project_id}`, { handed_off_to: 'agm-host' })
  for (const [method, action, body] of ACTIONS) {
    const e = await refused(m.request(method as 'POST', `/bots/${bot.id}/${action}`, body))
    assert.equal(e.status, 409, action)
    assert.equal(e.body.reason, 'handed_off', action)
    assert.equal(e.body.bot_id, bot.id, action)
    assert.equal(e.body.handed_off_to, 'agm-host', action)
    assert.equal(typeof e.body.message, 'string', `${action}：daemon 附一句人話，畫面靠它`)
  }
})

test('已移交的專案：群組訊息每顆都跳過，reason 是 conflict、detail 是 handed_off', async () => {
  const m = new MockTransport()
  const bot = (await bots(m)).find((b) => !b.parent_bot_id)!
  await m.request('PATCH', `/projects/${bot.project_id}`, { handed_off_to: 'agm-host' })
  const res = (await m.request('POST', `/projects/${bot.project_id}/chat`, { text: '@all hi', client_request_id: 'g-1' })) as {
    sent: unknown[]
    skipped: { reason: string; detail: string }[]
  }
  assert.equal(res.sent.length, 0)
  assert.ok(res.skipped.length > 0)
  for (const s of res.skipped) {
    assert.equal(s.reason, 'conflict')
    assert.equal(s.detail, 'handed_off')
  }
})

test('收回（handed_off_to: null）之後照常動作', async () => {
  const m = new MockTransport()
  const bot = (await bots(m)).find((b) => !b.parent_bot_id && !b.run)!
  await m.request('PATCH', `/projects/${bot.project_id}`, { handed_off_to: 'agm-host' })
  await m.request('PATCH', `/projects/${bot.project_id}`, { handed_off_to: null })
  const r = (await m.request('POST', `/bots/${bot.id}/start`)) as { run_id: string }
  assert.ok(r.run_id)
})
