/** mock 的分享事件要跟 daemon 同形（API.md §8）：`bot_share_changed` 帶 `allow_embed`；輪換連結只換 token，不動 `allow_embed`（daemon 的 `rotate` 只寫 token 欄位）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { manualMockClock, useManualMockClock } from './mockClock.ts'

interface Frame { type: string; data?: Record<string, unknown> }

// 開 socket 要等 mock 的 120ms 上線延遲：用虛擬時間，不在牆鐘上等。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()

const collect = async () => {
  const frames: Frame[] = []
  const close = mock.openSocket({ since: () => 0, onStatus: () => {}, onFrame: (f) => frames.push(f as Frame) })
  await manualMockClock.advance(200)
  return { frames, close }
}

test('受限分享輪換：bot_share_changed 帶 allow_embed:false（受限不能嵌入）', async () => {
  const st = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; share_enabled: boolean }[] }[] }
  const shared = st.projects.flatMap((p) => p.bots).find((b) => b.share_enabled)
  assert.ok(shared, 'seed 要有一顆已開分享的 bot')
  const { frames, close } = await collect()
  await mock.request('POST', `/bots/${shared.id}/share/rotate`, {})
  close()
  const ev = frames.find((f) => f.type === 'bot_share_changed')
  assert.ok(ev, '輪換要推 bot_share_changed')
  assert.equal(ev.data?.bot_id, shared.id)
  assert.equal(ev.data?.allow_embed, false)
})

test('信任分享輪換：allow_embed 保留 true，事件也帶 true', async () => {
  const st = (await mock.request('GET', '/state')) as { projects: { id: string; bots: { id: string }[] }[] }
  const pid = st.projects[0].id
  const made = (await mock.request('POST', `/projects/${pid}/bots`, {
    name: 'trusted-embed', kind: 'claude', share_profile: 'trusted', confirm_trusted: true,
  })) as { bot_id: string }
  const botId = made.bot_id
  await mock.request('POST', `/bots/${botId}/share`, { enabled: true, allow_embed: true })
  const { frames, close } = await collect()
  await mock.request('POST', `/bots/${botId}/share/rotate`, {})
  close()
  const ev = frames.find((f) => f.type === 'bot_share_changed')
  assert.ok(ev, '輪換要推 bot_share_changed')
  assert.equal(ev.data?.allow_embed, true)
  const view = (await mock.request('GET', `/bots/${botId}/share`)) as { allow_embed: boolean }
  assert.equal(view.allow_embed, true)
})
