/**
 * 主力晶片的快取倒數（SPEC §6.5j）：底色本身就是倒數——只加 class 與 `--cache-fill`，不多一個元素。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const minAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString()

function seed() {
  const bot = (id: string, kind: string, primary: boolean) => ({
    id, name: id, project_id: 'p1', kind, identity: null, model: null, effort: null, args: [], env: {}, autostart: false,
    inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary, primary_position: 0, cwd: null,
  })
  const bots = [bot('fresh', 'claude', true), bot('warn', 'codex', true), bot('low', 'claude', true), bot('cold', 'claude', true), bot('busy', 'claude', true), bot('grok', 'grok', true), bot('other', 'claude', false)]
  const run = (id: string, status: string, last: string | null, ttl: number | null) => ({
    id: `r-${id}`, bot_id: id, state: 'running', agent_status: status, pane_id: 'w1:p1', started_at: minAgo(300), agent_status_since: minAgo(300),
    last_api_at: last, cache_ttl_secs: ttl,
  })
  const runs = {
    fresh: run('fresh', 'idle', minAgo(15), 3600),
    warn: run('warn', 'idle', minAgo(50), 3600),
    low: run('low', 'idle', minAgo(57), 3600),
    cold: run('cold', 'idle', minAgo(90), 3600),
    busy: run('busy', 'working', minAgo(200), 3600),
    grok: run('grok', 'idle', minAgo(1), null),
    other: run('other', 'working', minAgo(1), 3600),
  }
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj', path: '/p', host: 'local' }], bots, runs, botOrder: {}, projectOrder: [], botUnread: {},
    hiddenBotIds: [], selectedBotId: null, supervisorProjectId: null, connected: true, socket: 'open',
  } as never)
}

const chip = (id: string) => document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${id}"]`)!

test('主力晶片：底色依剩餘時間填色、綠黃紅分級；已涼回一般底色並在 tooltip 標已涼', async () => {
  seed()
  await mount(<UnreadChip />)
  assert.ok(chip('fresh').classList.contains('cache-fresh'))
  assert.equal(chip('fresh').style.getPropertyValue('--cache-fill'), '75.0%')
  assert.match(chip('fresh').title, /快取約 45 分後到期（上次活動 \d\d:\d\d）/)
  assert.ok(chip('warn').classList.contains('cache-warn'))
  assert.ok(chip('low').classList.contains('cache-low'))
  // 已涼：沒有任何 cache- class、沒有填色，tooltip 講已涼。
  assert.ok(![...chip('cold').classList].some((c) => c.startsWith('cache-')))
  assert.equal(chip('cold').style.getPropertyValue('--cache-fill'), '')
  assert.match(chip('cold').title, /快取已涼/)
  // 回合進行中＝滿條。
  assert.ok(chip('busy').classList.contains('cache-fresh'))
  assert.equal(chip('busy').style.getPropertyValue('--cache-fill'), '100.0%')
  // grok 不顯示。
  assert.ok(![...chip('grok').classList].some((c) => c.startsWith('cache-')))
  assert.doesNotMatch(chip('grok').title, /快取/)
  // 非主力（沒釘、在跑）不畫。
  const other = [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((el) => el.textContent?.includes('other'))!
  assert.ok(![...other.classList].some((c) => c.startsWith('cache-')))
})

test('不多加元素：有倒數與已涼的晶片子元素結構一樣', async () => {
  seed()
  await mount(<UnreadChip />)
  const shape = (el: HTMLElement) => [...el.children].map((c) => c.className).join('|')
  assert.equal(shape(chip('fresh')), shape(chip('cold')))
})
