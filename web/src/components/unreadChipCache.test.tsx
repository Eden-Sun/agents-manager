/**
 * 主力晶片的快取倒數（SPEC §6.5j）：底色本身就是倒數——只加 class 與 `--cache-fill`，不多一個元素。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
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
  const bots = [bot('fresh', 'claude', true), bot('warn', 'claude', true), bot('low', 'claude', true), bot('cold', 'claude', true), bot('busy', 'claude', true), bot('grok', 'grok', true), bot('other', 'claude', false)]
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
/** 電腦不再 hover 開狀態卡（2026-10-08）：快取倒數只在原生 tooltip（`title`）裡；hover 之後也不該冒出狀態卡。 */
async function hoverText(id: string): Promise<string> {
  const el = chip(id)
  // 以前 hover 開卡有一段延遲：換成手動時鐘走 450ms（hover 排的任何 timer 都會到期），不睡牆鐘。
  const timers = installManualTimers()
  try {
    await act(async () => {
      el.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }))
      el.dispatchEvent(new MouseEvent('mouseenter', { bubbles: false }))
      await timers.clock.advance(450)
    })
  } finally {
    timers.restore()
  }
  assert.equal(document.querySelector('.bot-status-card'), null, 'hover 不該開狀態卡')
  const text = el.title
  await act(async () => {
    el.dispatchEvent(new MouseEvent('mouseout', { bubbles: true }))
    el.dispatchEvent(new MouseEvent('mouseleave', { bubbles: false }))
  })
  return text
}

test('主力晶片：底色依剩餘時間填色、綠黃紅分級；已涼回一般底色並在 tooltip 標已涼', async () => {
  seed()
  await mount(<UnreadChip />)
  assert.ok(chip('fresh').classList.contains('cache-fresh'))
  assert.equal(chip('fresh').style.getPropertyValue('--cache-fill'), '75.0%')
  assert.match(await hoverText('fresh'), /快取約 45 分後到期（上次活動 (?:\d+\/\d+ )?\d\d:\d\d）/)
  assert.ok(chip('warn').classList.contains('cache-warn'))
  assert.ok(chip('low').classList.contains('cache-low'))
  // 已涼：沒有任何 cache- class、沒有填色，tooltip 講已涼。
  assert.ok(![...chip('cold').classList].some((c) => c.startsWith('cache-')))
  assert.equal(chip('cold').style.getPropertyValue('--cache-fill'), '')
  assert.match(await hoverText('cold'), /快取已涼/)
  // 回合進行中＝滿條。
  assert.ok(chip('busy').classList.contains('cache-fresh'))
  assert.equal(chip('busy').style.getPropertyValue('--cache-fill'), '100.0%')
  // grok 不顯示。
  assert.ok(![...chip('grok').classList].some((c) => c.startsWith('cache-')))
  assert.doesNotMatch(await hoverText('grok'), /快取/)
  // 非主力（沒釘、在跑）不畫。
  const other = [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((el) => el.textContent?.includes('other'))!
  assert.ok(![...other.classList].some((c) => c.startsWith('cache-')))
})

test('codex 主力晶片：未知用量維持淡藍、標明未知，不多加元素', async () => {
  seed()
  const s = useStore.getState()
  const codexRun = { ...s.runs.warn, cache_ttl_secs: null }
  useStore.setState({ bots: s.bots.map((b) => b.id === 'warn' ? { ...b, kind: 'codex' } : b), runs: { ...s.runs, warn: codexRun } } as never)
  await mount(<UnreadChip />)
  const el = chip('warn')
  assert.ok(el.classList.contains('cache-codex'), '帶 .cache-codex class')
  assert.ok(!el.classList.contains('cache-warn') && !el.classList.contains('cache-fresh'), '不帶倒數等級 class')
  assert.equal(el.style.getPropertyValue('--cache-fill'), '0.0%', '未知時不假裝滿格')
  assert.match(el.title, /context.*未知/)
  // 子元素結構與一般主力晶片一致
  const shape = (e: HTMLElement) => [...e.children].map((c) => c.className).join('|')
  assert.equal(shape(el), shape(chip('cold')), '不多加元素、不改尺寸結構')
})

test('不多加元素：有倒數與已涼的晶片子元素結構一樣', async () => {
  seed()
  await mount(<UnreadChip />)
  const shape = (el: HTMLElement) => [...el.children].map((c) => c.className).join('|')
  assert.equal(shape(chip('fresh')), shape(chip('cold')))
})

test('不用保溫（keep_warm_skip）：晶片不畫倒數底色、tooltip 講不保溫中；取消後即時恢復', async () => {
  seed()
  await mount(<UnreadChip />)
  assert.ok(chip('fresh').classList.contains('cache-fresh'))
  const setSkip = (skip: boolean) =>
    act(async () => {
      const s = useStore.getState()
      useStore.setState({ runs: { ...s.runs, fresh: { ...s.runs.fresh, keep_warm_skip: skip } } } as never)
    })
  await setSkip(true)
  assert.ok(![...chip('fresh').classList].some((c) => c.startsWith('cache-')), '沒有 cache- class')
  assert.equal(chip('fresh').style.getPropertyValue('--cache-fill'), '')
  const text = await hoverText('fresh')
  assert.match(text, /不保溫：cache 到期前會自動壓縮（context 夠大時）/)
  assert.doesNotMatch(text, /快取約/)
  await setSkip(false)
  assert.ok(chip('fresh').classList.contains('cache-fresh'))
  assert.equal(chip('fresh').style.getPropertyValue('--cache-fill'), '75.0%')
  assert.match(await hoverText('fresh'), /快取約 45 分後到期/)
})


test('codex 主力依實際 session token 比例填色，超界與缺值安全處理', async () => {
  seed()
  const s = useStore.getState()
  useStore.setState({ bots: s.bots.map((b) => b.id === 'warn' ? { ...b, kind: 'codex' } : b), runs: { ...s.runs, warn: { ...s.runs.warn, cache_ttl_secs: null, prompt_cache: { context_used_tokens: 128000, context_size: 512000 } } } } as never)
  await mount(<UnreadChip />)
  assert.equal(chip('warn').style.getPropertyValue('--cache-fill'), '25.0%')
  assert.match(chip('warn').title, /context.*25/)
  await act(() => { const state = useStore.getState(); useStore.setState({ runs: { ...state.runs, warn: { ...state.runs.warn, prompt_cache: { context_used_tokens: 500, context_size: 100 } } } } as never) })
  assert.equal(chip('warn').style.getPropertyValue('--cache-fill'), '100.0%')
  await act(() => { const state = useStore.getState(); useStore.setState({ runs: { ...state.runs, warn: { ...state.runs.warn, prompt_cache: { context_used_tokens: 0, context_size: 100 } } } } as never) })
  assert.equal(chip('warn').style.getPropertyValue('--cache-fill'), '0.0%')
  assert.doesNotMatch(chip('warn').title, /context.*未知/)
  await act(() => { const state = useStore.getState(); useStore.setState({ runs: { ...state.runs, warn: { ...state.runs.warn, prompt_cache: { context_used_tokens: -1, context_size: 100 } } } } as never) })
  assert.equal(chip('warn').style.getPropertyValue('--cache-fill'), '0.0%')
  assert.match(chip('warn').title, /context.*未知/)
})
