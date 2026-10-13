/** mock 的「打斷」要跟 daemon 同形：帶的 `turn_id` 已不在飛 → 409 `turn_not_in_flight`，不動在飛的那一筆（API.md §4）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { manualMockClock, useManualMockClock } from './mockClock.ts'

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

test('打斷帶的 turn_id 不在飛：409 turn_not_in_flight，在飛的那一筆不動', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '打斷測試', client_request_id: 'int-1' })) as { turn_id: string }
  await assert.rejects(
    mock.request('POST', `/bots/${id}/interrupt`, { turn_id: 'turn_gone' }),
    (e: unknown) => e instanceof ApiError && e.status === 409 && e.message === 'turn_not_in_flight',
  )
  // 在飛的那一筆沒被標 failed：放棄它照樣收得下（同 daemon）。
  await mock.request('POST', `/turns/${out.turn_id}/abandon`, {})
})
