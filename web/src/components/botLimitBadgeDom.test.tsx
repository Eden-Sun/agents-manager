/**
 * #1899：主力 bot 撞限（daemon `bot.limit_hit`）時側欄頂層列掛「到限」晶片，title 帶重置時間；
 * 沒有撞限、已過 until、child 列都不畫。真的掛 `Sidebar` 進 happy-dom。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { limitHitWhen } from '../lib/limitHitWhen'
import { useStore } from '../store/store'
import { BotLimitBadge } from './BotLimitBadge'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const FUTURE = new Date(Date.now() + 6 * 3600_000).toISOString()
const PAST = new Date(Date.now() - 3600_000).toISOString()
const LIMIT = { message: "You've hit your usage limit", until: FUTURE, at: PAST, bucket: null }

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, parent: string | null, limit_hit: unknown = null) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'codex', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: parent ? 'child' : 'user', parent_bot_id: parent,
  primary: false, primary_position: 0, cwd: null, limit_hit,
})
const badges = () => [...document.querySelectorAll<HTMLElement>('.bot-limit-hit')]

beforeEach(() => {
  localStorage.clear()
  fakeApi()
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('hit', null, LIMIT), bot('ok', null), bot('kid', 'hit', LIMIT)],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'ok', selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('頂層主力撞限時掛「到限」，title 帶重置時間；沒撞限的列與 child 不畫', async () => {
  await mount(<Sidebar />)
  await settle()
  const list = badges()
  assert.equal(list.length, 1, '只有頂層撞限的那一列掛晶片')
  assert.equal(list[0].closest('.bot-row')?.getAttribute('data-bot-id'), 'hit')
  assert.equal(list[0].textContent, '到限')
  assert.match(list[0].title, new RegExp(`codex · 預設 已達用量上限，${escapeRe(limitHitWhen(FUTURE)!)} 恢復`))
  assert.match(list[0].title, /You've hit your usage limit/)
})

test('limit_hit 為 null 或已過 until 時不畫', async () => {
  useStore.setState({ bots: [bot('hit', null, null), bot('ok', null, { ...LIMIT, until: PAST })] } as never)
  await mount(<Sidebar />)
  await settle()
  assert.equal(badges().length, 0)
})

test('BotLimitBadge：沒寫 until 照掛、title 說 CLI 沒寫時間', async () => {
  await mount(<BotLimitBadge hit={{ message: '擋住了', until: null }} who="Codex · cc1" />)
  const el = document.querySelector<HTMLElement>('.bot-limit-hit')
  assert.ok(el, '沒寫 until 仍要掛')
  assert.match(el!.title, /Codex · cc1 已達用量上限，恢復時間 CLI 沒寫/)
})

function escapeRe(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&')
}
