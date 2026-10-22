/**
 * #1899：主力晶片的撞限角標。掛真的 `UnreadChip`，資料從 `GET /api/state` 的 fixture 經 `refreshState` 進來
 * （不直接塞 store，才會走 normalize 的 toBot）。角標只在主力列、還沒過 `until` 時畫；文字併進晶片 label。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { limitHitWhen } from '../lib/limitHitWhen'
import { resetStoreForTest, useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const FUTURE = new Date(Date.now() + 6 * 3600_000).toISOString()
const PAST = new Date(Date.now() - 3600_000).toISOString()
const HIT = { message: "You've hit your usage limit", until: FUTURE }

/** daemon 的 bot 形狀（`/api/state` 裡一顆）。 */
const rawBot = (id: string, extra: Record<string, unknown>) => ({
  id,
  project_id: 'p1',
  name: id,
  kind: 'codex',
  model: null,
  effort: null,
  args: [],
  env: {},
  autostart: false,
  inject_hooks: true,
  auto_approve: false,
  identity: null,
  herdr_session: null,
  managed_by: 'user',
  parent_bot_id: null,
  primary: false,
  primary_position: 0,
  cwd: null,
  limit_hit: null,
  ...extra,
})

const bots = [
  rawBot('hit', { primary: true, primary_position: 1, limit_hit: HIT }),
  rawBot('old', { primary: true, primary_position: 2, limit_hit: { message: 'x', until: PAST } }),
  rawBot('ok', { primary: true, primary_position: 3 }),
  rawBot('nt', { primary: true, primary_position: 4, limit_hit: { message: 'x', until: null } }),
  // child 不在主力列；就算有撞限也不畫角標（daemon 也不會給 child 撞限）。
  rawBot('kid', { parent_bot_id: 'hit', managed_by: 'child', limit_hit: HIT }),
]

const chip = (id: string) => document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${id}"]`)
const mark = (id: string) => chip(id)?.querySelector<HTMLElement>('.unread-chip-limit') ?? null

async function openWithState() {
  fakeApi((req) => (req.path === '/api/state' ? { daemon_seq: 1, projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local', bots }], bots, runs: [], turns: [] } : {}))
  resetStoreForTest()
  await useStore.getState().refreshState()
  await mount(<UnreadChip />)
  await settle()
}

test('撞限中的主力：右上角有角標，title 與 sr-only label 都寫「已達用量上限，<時間> 恢復」', async () => {
  await openWithState()
  const m = mark('hit')
  assert.ok(m, '撞限中的主力要有角標')
  assert.equal(m.getAttribute('aria-hidden'), 'true', '角標本身不讀，文字走晶片 label')
  const text = `codex 已達用量上限，${limitHitWhen(FUTURE)} 恢復`
  assert.match(chip('hit')!.title, new RegExp(text.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&')))
  assert.ok(chip('hit')!.querySelector('.sr-only')?.textContent?.includes(text), '螢幕閱讀器念到撞限')
})

test('過了 until、沒撞限的主力、child：都沒有角標', async () => {
  await openWithState()
  assert.equal(mark('old'), null, '已過 until 不畫')
  assert.equal(mark('ok'), null, '沒撞限不畫')
  assert.equal(mark('kid'), null, 'child 不畫（不在主力列）')
  assert.ok(!chip('old')!.title.includes('已達用量上限'), '過期的 title 也不講撞限')
})

test('沒寫 until 的撞限照掛，文字說 CLI 沒寫恢復時間', async () => {
  await openWithState()
  assert.ok(mark('nt'), '沒寫 until 仍要掛角標')
  assert.match(chip('nt')!.title, /codex 已達用量上限，恢復時間 CLI 沒寫/)
})
