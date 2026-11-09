/**
 * codex bot 設定的「基準快照」：鎖定目前 context、清除、顯示鎖定時間；只有 codex bot 看得到。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BaseSnapshotControls } from './BaseSnapshotControls'
import { BotSettingsPanel } from './BotSettingsPanel'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const btn = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('.base-snapshot-controls button')].find((b) => b.textContent?.trim() === label)!
const status = () => document.querySelector('.base-snapshot-status')?.textContent ?? ''

test('鎖定與清除：打對端點，並顯示鎖定時間', async () => {
  let locked = false
  const requests = fakeApi((req) => {
    if (!req.path.endsWith('/bots/b1/base-snapshot')) return undefined
    if (req.method === 'PUT') locked = true
    if (req.method === 'DELETE') locked = false
    return locked ? { locked: true, session_id: 'sid', locked_at: '2030-01-01T00:00:00.000Z' } : { locked: false }
  })
  await mount(<BaseSnapshotControls botId="b1" name="one" />)
  await settle()
  assert.match(status(), /尚未鎖定基準/)
  assert.equal(btn('清除基準').disabled, true)
  await click(btn('鎖定目前 context 為基準'))
  await settle()
  assert.ok(requests.some((r) => r.method === 'PUT' && r.path.endsWith('/bots/b1/base-snapshot')))
  assert.match(status(), /已鎖定於/)
  assert.equal(btn('清除基準').disabled, false)
  await click(btn('清除基準'))
  await settle()
  assert.ok(requests.some((r) => r.method === 'DELETE'))
  assert.match(status(), /尚未鎖定基準/)
})

const mk = (kind: string) =>
  ({ id: 'b1', name: 'one', project_id: 'p1', kind, model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false }) as unknown as Bot

for (const [kind, shown] of [['codex', true], ['claude', false]] as const) {
  test(`bot 設定面板：${kind} bot ${shown ? '有' : '沒有'}基準快照區塊`, async () => {
    fakeApi(() => ({ locked: false }))
    useStore.setState({ bots: [mk(kind)], runs: {}, busy: {}, selectedBotId: 'b1', settingsBotId: 'b1', refreshState: async () => {} })
    await mount(<BotSettingsPanel botId="b1" />)
    await settle()
    assert.equal(document.querySelector('.base-snapshot-controls') !== null, shown)
  })
}
