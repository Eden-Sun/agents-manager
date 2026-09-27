/** mock 的「補充」「插隊」要跟 daemon 同形：補充帶 `record` 才記、掛在進行中的回合；插隊真的打斷才標。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'

interface Msg { id: string; role: string; turn_id: string | null; content: string; sent_via?: string | null }
interface BotRow { id: string; kind: string; run?: { state?: string } | null }

const mock = new MockTransport()
const bots = async () => ((await mock.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)
let started: Promise<string> | null = null
/** seed 的 bot 都沒在跑：啟動一顆 claude，等 mock 把它推到 running。 */
const claudeBot = () =>
  (started ??= (async () => {
    const b = (await bots()).find((x) => x.kind === 'claude')
    assert.ok(b, 'seed 要有 claude')
    await mock.request('POST', `/bots/${b.id}/start`, {})
    for (let i = 0; i < 100; i++) {
      if ((await bots()).find((x) => x.id === b.id)?.run?.state === 'running') return b.id
      await new Promise((r) => setTimeout(r, 50))
    }
    throw new Error('mock 的 claude 沒起來')
  })())
const messages = async (botId: string, qs: string) =>
  ((await mock.request('GET', `/bots/${botId}/messages${qs}`)) as { messages: Msg[] }).messages

test('補充帶 record：記成進行中回合的使用者訊息；不帶就不留', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: 'retry 跑久一點', client_request_id: 'sv-1' })) as { turn_id: string }
  const plain = (await mock.request('POST', `/bots/${id}/text`, { text: '1', enter: true })) as Record<string, unknown>
  assert.equal(plain.message_id, undefined)
  const rec = (await mock.request('POST', `/bots/${id}/text`, { text: '順便看 log', enter: true, record: true })) as { message_id: string | null }
  assert.ok(rec.message_id)
  const users = await messages(id, `?turn_id=${out.turn_id}&role=user`)
  assert.deepEqual(
    users.map((m) => [m.content, m.sent_via ?? null]),
    [['retry 跑久一點', null], ['順便看 log', 'supplement']],
  )
})

test('插隊真的打斷進行中的回合才標 send_now', async () => {
  const id = await claudeBot()
  // 先確定有一輪在跑（上一條的那輪還沒結束的話這句 409，一樣是在跑）。
  await mock.request('POST', `/bots/${id}/prompt`, { text: 'retry 再跑一輪', client_request_id: 'sv-busy' }).catch(() => {})
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '改做這個', client_request_id: 'sv-2', send_now: true })) as { turn_id: string }
  const [m] = await messages(id, `?turn_id=${out.turn_id}&role=user`)
  assert.equal(m.sent_via, 'send_now')
})
