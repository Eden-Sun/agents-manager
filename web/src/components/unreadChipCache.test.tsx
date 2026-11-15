/**
 * 主力晶片的快取倒數（SPEC §6.5j）：底色本身就是倒數——只加 class 與 `--cache-fill`，不多一個元素。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { clearApiBotEntriesForTest, setApiBotCacheForTest } from '../hooks/useApiTurnCosts'
import { useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(async () => {
  clearApiBotEntriesForTest()
  await unmountAll()
})
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

test('claude-api 主力晶片：底色依 expires_at 與 ttl 倒數填色、綠黃紅分級；樣式與 CLI bot 完全一致', async () => {
  seed()
  const bot = (id: string, primary: boolean) => ({
    id, name: id, project_id: 'p1', kind: 'claude-api', identity: null, model: 'claude-opus-5-5', effort: null, args: [], env: {}, autostart: false,
    inject_hooks: false, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary, primary_position: 0, cwd: null,
  })
  const apiBots = [
    bot('api-fresh', true),
    bot('api-warn', true),
    bot('api-low', true),
    bot('api-cold', true),
    bot('api-busy', true),
    bot('api-other', false),
  ]
  const s = useStore.getState()
  useStore.setState({
    bots: [...s.bots, ...apiBots],
    botUnread: { ...s.botUnread, 'api-other': 1 },
  } as never)

  const expFromNow = (mins: number) => new Date(Date.now() + mins * 60_000).toISOString()
  setApiBotCacheForTest('api-fresh', { last_request_at: minAgo(15), ttl_secs: 3600, expires_at: expFromNow(45), working: false })
  setApiBotCacheForTest('api-warn', { last_request_at: minAgo(50), ttl_secs: 3600, expires_at: expFromNow(10), working: false })
  setApiBotCacheForTest('api-low', { last_request_at: minAgo(57), ttl_secs: 3600, expires_at: expFromNow(3), working: false })
  setApiBotCacheForTest('api-cold', { last_request_at: minAgo(90), ttl_secs: 3600, expires_at: minAgo(30), working: false })
  setApiBotCacheForTest('api-busy', { last_request_at: minAgo(200), ttl_secs: 3600, expires_at: expFromNow(10), working: true })
  setApiBotCacheForTest('api-other', { last_request_at: minAgo(1), ttl_secs: 3600, expires_at: expFromNow(59), working: true })

  await mount(<UnreadChip />)

  // fresh: 45 / 60 = 75.0%
  assert.ok(chip('api-fresh').classList.contains('cache-fresh'))
  assert.equal(chip('api-fresh').style.getPropertyValue('--cache-fill'), '75.0%')
  assert.match(await hoverText('api-fresh'), /快取約 45 分後到期（上次活動 (?:\d+\/\d+ )?\d\d:\d\d）/)

  // warn: 10 / 60
  assert.ok(chip('api-warn').classList.contains('cache-warn'))

  // low: 3 / 5 = 60.0%
  assert.ok(chip('api-low').classList.contains('cache-low'))
  assert.equal(chip('api-low').style.getPropertyValue('--cache-fill'), '60.0%')

  // cold: 回一般底色，沒有 cache- class，沒有填色，tooltip 標快取已涼
  assert.ok(![...chip('api-cold').classList].some((c) => c.startsWith('cache-')))
  assert.equal(chip('api-cold').style.getPropertyValue('--cache-fill'), '')
  assert.match(await hoverText('api-cold'), /快取已涼/)

  // working: 滿條
  assert.ok(chip('api-busy').classList.contains('cache-fresh'))
  assert.equal(chip('api-busy').style.getPropertyValue('--cache-fill'), '100.0%')
  assert.match(await hoverText('api-busy'), /回合進行中，快取是熱的/)

  // 非主力（沒釘）不畫
  const other = [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((el) => el.textContent?.includes('api-other'))!
  assert.ok(![...other.classList].some((c) => c.startsWith('cache-')))

  // 子元素結構與 CLI 主力完全一致，不多加元素
  const shape = (e: HTMLElement) => [...e.children].map((c) => c.className).join('|')
  assert.equal(shape(chip('api-fresh')), shape(chip('fresh')))
  assert.equal(shape(chip('api-cold')), shape(chip('cold')))
})

/**
 * claude-api 沒有 pane／hook，daemon 用回合狀態寫 `run.agent_status`（開始 working、結束 idle）；前端膠囊跟 CLI bot 走同一條：
 * working＝轉圈，已回答的未讀＝數字徽章。沒有 pane_id 也一樣（不分支）。
 */
test('claude-api 主力膠囊：回合進行中（agent_status working、無 pane）轉圈；回答完（idle、未讀 1）有未讀徽章、沒有轉圈', async () => {
  const bot = (id: string) => ({
    id, name: id, project_id: 'p1', kind: 'claude-api', identity: null, model: null, effort: null, args: [], env: {}, autostart: false,
    inject_hooks: false, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: true, primary_position: 0, cwd: null,
  })
  const run = (id: string, status: string) => ({ id: `r-${id}`, bot_id: id, state: 'running', agent_status: status, pane_id: null, started_at: minAgo(60), agent_status_since: minAgo(1) })
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj', path: '/p', host: 'local' }],
    bots: [bot('api-run'), bot('api-done')],
    runs: { 'api-run': run('api-run', 'working'), 'api-done': run('api-done', 'idle') },
    botOrder: {}, projectOrder: [], botUnread: { 'api-done': 1 }, hiddenBotIds: [], selectedBotId: null, supervisorProjectId: null, connected: true, socket: 'open',
  } as never)
  await mount(<UnreadChip />)
  const ring = (id: string) => chip(id).querySelector('.unread-chip-working-ring')
  assert.ok(ring('api-run'), '進行中：轉圈')
  assert.equal(ring('api-done'), null, '回答完：不轉')
  assert.equal(chip('api-done').querySelector('.unread-chip-n')?.textContent, '1', '已回答未讀：徽章')
  assert.equal(chip('api-run').querySelector('.unread-chip-n'), null)
  // 使用者點進去看過就清掉（跟 CLI bot 同一個帳）。
  await act(async () => { useStore.setState({ botUnread: {} } as never) })
  assert.equal(chip('api-done').querySelector('.unread-chip-n'), null)
})
