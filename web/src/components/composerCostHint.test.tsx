/** 輸入框上方的 context／快取提示：render 與不擋送出。 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { toStatusInfo } from '../api/normalize'
import { ComposerCostHint } from './ComposerCostHint'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const minAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString()

function seed(last: string | null, ttl: number | null, status = 'idle', withStatus = true, kind = 'claude') {
  useStore.setState({
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind, identity: null, model: null, effort: null, args: [], env: {}, autostart: false }],
    runs: {
      b1: {
        id: 'r1', bot_id: 'b1', state: 'running', agent_status: status, pane_id: 'w1:p1', started_at: minAgo(300), agent_status_since: minAgo(300),
        last_api_at: last, cache_ttl_secs: ttl,
        status: withStatus ? { context_used_pct: 45, context_used_tokens: 450_000, context_size: 1_000_000 } : null,
      },
    },
    turns: {}, messages: {}, busy: {},
  } as never)
}
const el = () => document.querySelector<HTMLElement>('.composer-cost')

test('還熱：一般色、寫剩幾分', async () => {
  seed(minAgo(10), 3600)
  await mount(<ComposerCostHint botId="b1" />)
  assert.match(el()!.textContent!, /context 45%（約 450K）/)
  assert.match(el()!.textContent!, /快取還熱（剩 50 分）/)
  assert.ok(!el()!.classList.contains('cold'))
})

test('已過期：cold 警示、說明重讀整段 context；不含任何 button', async () => {
  seed(minAgo(125), 3600)
  await mount(<ComposerCostHint botId="b1" />)
  assert.ok(el()!.classList.contains('cold'))
  assert.match(el()!.textContent!, /快取已過期（閒置 2 小時 5 分）/)
  assert.equal(el()!.querySelectorAll('button').length, 0)
})

test('回合中不顯示過期；grok 不顯示快取段；什麼都沒有就不畫', async () => {
  seed(minAgo(500), 3600, 'working')
  await mount(<ComposerCostHint botId="b1" />)
  assert.doesNotMatch(el()!.textContent!, /已過期/)
  await unmountAll()
  seed(minAgo(1), null)
  await mount(<ComposerCostHint botId="b1" />)
  assert.doesNotMatch(el()!.textContent!, /快取/)
  assert.match(el()!.textContent!, /context 45%/)
  await unmountAll()
  seed(minAgo(1), null, 'idle', true, 'grok') // grok 整段不畫
  await mount(<ComposerCostHint botId="b1" />)
  assert.equal(el(), null)
  await unmountAll()
  seed(null, null, 'idle', false)
  await mount(<ComposerCostHint botId="b1" />)
  assert.equal(el(), null)
})

test('claude 有 prompt_cache：以它為準（冷＝寫 recache_tokens_if_cold）；沒有才退回推算', async () => {
  const nowS = Math.floor(Date.now() / 1000)
  const pc = (over: object) => ({ context_window: { used_percentage: 45, total_input_tokens: 450_000 }, prompt_cache: { warm: false, expires_at: nowS - 600, ttl: '1h', recache_tokens_if_cold: 459_258, ...over } })
  seed(minAgo(1), 3600, 'idle', false) // last_api_at 說還熱，prompt_cache 說冷：以後者為準
  useStore.setState((s) => ({ runs: { b1: { ...s.runs.b1, status: toStatusInfo(pc({})) } } }) as never)
  await mount(<ComposerCostHint botId="b1" />)
  assert.match(el()!.textContent!, /快取已過期（閒置 1 小時 10 分），送出這則會重寫約 459K/)
  assert.ok(el()!.classList.contains('cold'))
})
