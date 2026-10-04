/** 快取狀態：狀態列「快取」欄常駐、輸入框只在冷時警示、不擋送出。 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { toPromptCache, toStatusInfo } from '../api/normalize'
import { ComposerCostHint } from './ComposerCostHint'
import { StatusCache } from './StatusCache'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const minAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString()
const nowS = () => Math.floor(Date.now() / 1000)

interface Seed {
  kind?: string
  last?: string | null
  ttl?: number | null
  agent?: string
  pc?: Record<string, unknown> | null
  status?: Record<string, unknown> | null
}
function seed({ kind = 'claude', last = minAgo(10), ttl = 3600, agent = 'idle', pc = null, status = null }: Seed = {}) {
  useStore.setState({
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind, identity: null, model: null, effort: null, args: [], env: {}, autostart: false }],
    runs: {
      b1: {
        id: 'r1', bot_id: 'b1', state: 'running', agent_status: agent, pane_id: 'w1:p1', started_at: minAgo(300), agent_status_since: minAgo(300),
        last_api_at: last, cache_ttl_secs: ttl, prompt_cache: pc ? toPromptCache(pc) : null, status: status ? toStatusInfo(status) : null,
      },
    },
    turns: {}, messages: {}, busy: {},
  } as never)
}
const claudeStatus = { context_window: { used_percentage: 45, context_window_size: 1_000_000 } }
const warn = () => document.querySelector<HTMLElement>('.composer-cost')
const cacheItem = () => document.querySelector<HTMLElement>('.sl-item[data-k="快取"]')

test('輸入框：熱的時候完全不顯示', async () => {
  seed({ pc: { source: 'statusline', warm: true, expires_at: nowS() + 1500, recache_tokens_if_cold: 459_258 }, status: claudeStatus })
  await mount(<ComposerCostHint botId="b1" />)
  assert.equal(warn(), null)
})

test('輸入框：冷了出警示（警示色 class、寫重寫量、沒有按鈕）；回合中不出', async () => {
  const cold = { source: 'statusline', warm: false, expires_at: nowS() - 3900, recache_tokens_if_cold: 811_000 }
  seed({ pc: cold, status: claudeStatus })
  await mount(<ComposerCostHint botId="b1" />)
  assert.ok(warn()!.classList.contains('cold'))
  assert.equal(warn()!.textContent, '快取已過期，送出這則會重寫約 811K')
  assert.equal(warn()!.querySelectorAll('button').length, 0)
  await unmountAll()
  seed({ pc: cold, status: claudeStatus, agent: 'working' })
  await mount(<ComposerCostHint botId="b1" />)
  assert.equal(warn(), null)
})

test('輸入框：退回推算（沒有 prompt_cache）冷了也警示；grok 不出', async () => {
  seed({ last: minAgo(125), status: claudeStatus })
  await mount(<ComposerCostHint botId="b1" />)
  assert.match(warn()!.textContent!, /^快取已過期，送出這則會重寫約 450K$/)
  await unmountAll()
  seed({ kind: 'grok', last: minAgo(125), ttl: null })
  await mount(<ComposerCostHint botId="b1" />)
  assert.equal(warn(), null)
})

test('狀態列：claude 有 context 時只接「快取」欄（熱／已冷）', async () => {
  const s = toStatusInfo(claudeStatus)
  seed({ pc: { source: 'statusline', warm: true, expires_at: nowS() + 1380 }, status: claudeStatus })
  await mount(<StatusCache botId="b1" status={s} />)
  assert.equal(cacheItem()!.textContent, '快取熱（剩 23 分）')
  assert.equal(document.querySelector('.sl-item[data-k="context"]'), null)
  await unmountAll()
  seed({ pc: { source: 'statusline', warm: false, expires_at: nowS() - 60 }, status: claudeStatus })
  await mount(<StatusCache botId="b1" status={s} />)
  assert.equal(cacheItem()!.textContent, '快取已冷')
})

test('狀態列：codex 補 context 並標「約」', async () => {
  seed({
    kind: 'codex', last: null,
    pc: { source: 'rollout_estimate', warm: true, expires_at: nowS() + 1200, recache_tokens_if_cold: 17_667, context_used_pct: 6.8, context_used_tokens: 17_667, context_size: 258_400 },
  })
  await mount(<StatusCache botId="b1" status={toStatusInfo({})} />)
  assert.equal(document.querySelector('.sl-item[data-k="context"]')!.textContent, 'context6.8% · 18k/258k')
  assert.equal(cacheItem()!.textContent, '快取熱（約剩 20 分）')
})

test('狀態列：grok 什麼都不加', async () => {
  seed({ kind: 'grok', ttl: null })
  await mount(<StatusCache botId="b1" status={toStatusInfo({})} />)
  assert.equal(document.querySelector('.sl-item'), null)
})
