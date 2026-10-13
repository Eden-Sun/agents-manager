/** mock 的 `start?resume=native`：接不回（這顆 bot 沒有記錄過的原生對話）→ 409 `cannot_resume`，不啟動（API.md §4）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { useManualMockClock } from './mockClock.ts'

interface BotRow { id: string; kind: string; parent_bot_id?: string | null; run?: { state?: string } | null }

// 不用虛擬時間：這條測試不等 bot 起來，連一個 timer 都不會排。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const bots = async () => ((await mock.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)

test('接回（resume=native）沒有記錄過的原生對話：409 cannot_resume 且不啟動', async () => {
  const b = (await bots()).find((x) => x.kind === 'claude' && !x.parent_bot_id)
  assert.ok(b, 'seed 要有頂層的 claude')
  await assert.rejects(
    mock.request('POST', `/bots/${b.id}/start?resume=native`, {}),
    (e: unknown) => e instanceof ApiError && e.status === 409 && e.message === 'cannot_resume',
  )
  assert.equal((await bots()).find((x) => x.id === b.id)?.run ?? null, null, '接不回不該留下 run')
})
