/** mock 的 `PATCH /api/bots/{id}` 要跟 daemon 同形：grok 只改 effort 不當場套用，要 `needs_restart:true`（API.md §10.2、SPEC §4.4a：`/effort` 會寫進 grok 的 config.toml，daemon 不送）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { manualMockClock, useManualMockClock } from './mockClock.ts'

interface BotRow { id: string; kind: string; run?: { state?: string } | null }

// mock 的 bot 啟動是 timer：用虛擬時間，不在牆鐘上輪詢等它。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const bots = async () => ((await mock.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)
/** seed 的 bot 都沒在跑：啟動指定 kind 的一顆，等 mock 把它推到 running。 */
const runningBot = async (kind: string) => {
  const b = (await bots()).find((x) => x.kind === kind)
  assert.ok(b, `seed 要有 ${kind}`)
  await mock.request('POST', `/bots/${b.id}/start`, {})
  for (let i = 0; i < 100; i++) {
    if ((await bots()).find((x) => x.id === b.id)?.run?.state === 'running') return b.id
    await manualMockClock.advance(250)
  }
  throw new Error(`mock 的 ${kind} 沒起來`)
}

test('grok 只改 effort：有在跑的 run 也要 needs_restart:true（不送 /effort）', async () => {
  const id = await runningBot('grok')
  const out = (await mock.request('PATCH', `/bots/${id}`, { effort: 'high' })) as { needs_restart: boolean }
  assert.equal(out.needs_restart, true)
})
