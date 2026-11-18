/**
 * openai-api 的 api-usage 語意（D2b-4，e56a84f70）：input 不含快取，快取讀寫分開帶；context.compact_at 為 null。
 * 網頁不得再扣快取（否則重複扣），也不得在沒有門檻時顯示「壓縮門檻」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiBotUsageBar } from './ApiBotUsageBar'
import { rawTransport } from '../api'
import { toApiBotUsage } from '../api/apiBotUsage'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

/** openai 一回合：OpenAI 原始 input 1700 = 不快取 1000 ＋ 快取讀 600 ＋ 快取寫 100；網頁收到的已是拆開的值。 */
const openAiUsage = {
  attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
  cost_microusd: { recorded: 50_000, reserved: 0, unknown: 0 },
  tokens: { input: 1000, output: 50, cache_read: 600, cache_creation: 100 },
  cache_hit_rate: 600 / 1700,
  last: { speed: 'standard', at: null, input_tokens: 1000, output_tokens: 50, cache_read_input_tokens: 600, cache_creation_input_tokens: 100 },
  context: { tokens: 1700, compact_at: null },
  turns: [],
}

test('toApiBotUsage：openai 的 input 原樣保留（不再扣快取），compact_at 為 null', () => {
  const u = toApiBotUsage(openAiUsage)
  assert.equal(u.tokens.input, 1000)
  assert.equal(u.tokens.cache_read, 600)
  assert.equal(u.tokens.cache_creation, 100)
  assert.equal(u.last?.input_tokens, 1000)
  assert.equal(u.last?.cache_read_tokens, 600)
  assert.equal(u.context.tokens, 1700)
  assert.equal(u.context.compact_at, null)
})

test('ApiBotUsageBar：compact_at 為 null 時不顯示壓縮門檻，也不標警示', async () => {
  useStore.setState({ turns: { 'oa-1': {} }, bots: [{ id: 'oa-1', kind: 'openai-api' }] } as never)
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) return openAiUsage
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<ApiBotUsageBar botId="oa-1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 30)) })
    const text = document.body.textContent ?? ''
    assert.doesNotMatch(text, /壓縮門檻/)
    assert.equal(document.querySelector('.sl-item.warn'), null, '沒有門檻就不該有 warn')
  } finally {
    rawTransport.request = orig
  }
})
