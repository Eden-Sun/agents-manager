/** mock 的「放棄回合」要跟 daemon 同形：只收 in-flight 或 delivery=unknown，已結束的回合 409 且不改寫（API.md §4，daemon #16）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { manualMockClock, useManualMockClock } from './mockClock.ts'

interface Msg { id: string; role: string; turn_id: string | null; content: string; keep_warm?: boolean }
interface BotRow { id: string; kind: string; run?: { state?: string } | null }

// mock 的 bot 啟動是 timer：用虛擬時間，不在牆鐘上輪詢等它。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
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
      await manualMockClock.advance(250)
    }
    throw new Error('mock 的 claude 沒起來')
  })())
const messages = async (botId: string, qs: string) =>
  ((await mock.request('GET', `/bots/${botId}/messages${qs}`)) as { messages: Msg[] }).messages

test('放棄一則已完成的回合：409 不改寫（daemon 只收 in-flight 或 delivery=unknown）', async () => {
  const id = await claudeBot()
  // 保溫那一輪是同步寫成 completed 的回合，正好拿來當「已結束」。
  mock.simulateKeepWarmReply(id)
  const turnId = (await messages(id, '?role=user')).find((m) => m.keep_warm)?.turn_id
  assert.ok(turnId, '保溫回合要有 turn_id')
  await assert.rejects(
    mock.request('POST', `/turns/${turnId}/abandon`, {}),
    (e: unknown) => e instanceof ApiError && e.status === 409 && e.message === 'turn is neither in-flight nor of unknown delivery',
  )
})

test('放棄進行中的回合照常收：200 並標 failed', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '放棄這一輪', client_request_id: 'ab-ok-1' })) as { turn_id: string }
  await mock.request('POST', `/turns/${out.turn_id}/abandon`, {})
  // 再放棄一次：已經不在飛了，應該是 409（不是再把 bot 設回 idle 或改寫）。
  await assert.rejects(mock.request('POST', `/turns/${out.turn_id}/abandon`, {}), (e: unknown) => e instanceof ApiError && e.status === 409)
})
