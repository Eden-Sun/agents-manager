/**
 * 全域 API 餘額在額度列上的呈現（使用者 2026-11-19）：
 * - 沒有任何額度格的主機（例如 m4p）仍畫出餘額晶片；兩個餘額都不可見才整條不畫；
 * - 額度選單底部「API 餘額」區列出 Claude／OpenAI 各一列（本機與遠端都有），點開同一個校正視窗；
 * - 手機（setOnly）時餘額區仍顯示；未設定／未啟用的那一列不畫。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { quotaKey } from '../api/types'
import { resetClaudeApiBalanceForTest } from '../hooks/useClaudeApiBalance'
import { resetOpenAiApiBalanceForTest } from '../hooks/useOpenAiApiBalance'
import { QuotaStrip } from './QuotaStrip'

/** 額度列要量版面、載入多個模組，預設 5 秒在滿載的 shard 上太緊。 */
const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

before(() => {
  // 同一個 shard 會跟別的檔共用模組狀態（別的檔可能留著沒收完的餘額讀取）：開頭先清一次，不靠前一個檔的 afterEach。
  resetClaudeApiBalanceForTest()
  resetOpenAiApiBalanceForTest()
  return setupDom()
})
afterEach(async () => {
  await unmountAll()
  resetClaudeApiBalanceForTest()
  resetOpenAiApiBalanceForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const claudeAnchored = {
  configured: true,
  anchor: { usd: 98.14, at: '2026-11-02T05:52:10.209Z' },
  spent_usd: 74.85,
  remaining_usd: 23.29,
  estimate: true,
  uncertain_usd: 0,
  remaining_floor_usd: 23.29,
  as_of: '2026-11-15T09:30:00.000Z',
}
const openaiAnchored = {
  enabled: true,
  configured: true,
  anchor: { usd: 20, at: '2026-11-10T00:00:00.000Z' },
  spent_usd: 2,
  remaining_usd: 18,
  uncertain_usd: 0,
  remaining_floor_usd: 18,
  as_of: '2026-11-19T01:00:00.000Z',
}

async function withBalances<T>(claude: unknown, openai: unknown, fn: () => Promise<T>): Promise<T> {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/balance') return claude
    if (path === '/openai-api/balance') return openai
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    return await fn()
  } finally {
    rawTransport.request = orig
  }
}

const settle = () => new Promise((r) => setTimeout(r, 30))
const claudeChip = () => document.querySelector<HTMLElement>('[data-k="quota-balance"]')
const openaiChip = () => document.querySelector<HTMLElement>('[data-k="openai-balance"]')

/** 本機的 Claude 額度格（讓額度選單打得開）。 */
function seedLocalClaudeGauge() {
  useStore.setState({
    quota: {
      [quotaKey('local', 'claude')]: {
        five_hour: null,
        seven_day: { used_pct: 40, low: false, critical: false, resets_at: new Date(Date.now() + 3_600_000).toISOString(), observed_at: null },
        fable: null,
        reset_credits: null,
        limit_hit: null,
        plan: null,
        updated_at: new Date().toISOString(),
        stale: false,
        host: 'local',
      },
    },
    identities: [],
  } as never)
}

/** m4p 的 Claude 額度格（遠端主機也要能開選單）。 */
function seedRemoteClaudeGauge(host: string) {
  useStore.setState({
    quota: {
      [quotaKey(host, 'claude')]: {
        five_hour: null,
        seven_day: { used_pct: 40, low: false, critical: false, resets_at: new Date(Date.now() + 3_600_000).toISOString(), observed_at: null },
        fable: null,
        reset_credits: null,
        limit_hit: null,
        plan: null,
        updated_at: new Date().toISOString(),
        stale: false,
        host,
      },
    },
    identities: [],
  } as never)
}

it('m4p 沒有任何額度格：仍畫出 Claude 與 OpenAI 餘額晶片，不畫量表', async () => {
  resetStoreForTest()
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => claudeChip() !== null && openaiChip() !== null, 'm4p 的餘額晶片畫出來')
    assert.equal(document.querySelectorAll('.quota-hp').length, 0, '沒有額度格就不畫量表')
    assert.ok(claudeChip() && openaiChip())
  })
})

it('m4p 沒有額度格、餘額也都不可見：整條額度列不畫', async () => {
  resetStoreForTest()
  await withBalances({ configured: false }, { enabled: false, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await settle()
    assert.equal(document.querySelector('.quota-strip'), null)
  })
})

it('本機：額度選單底部有「API 餘額」區，Claude 與 OpenAI 各一列，點了開各自的校正視窗', async () => {
  resetStoreForTest()
  seedLocalClaudeGauge()
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-bars-open') !== null, '本機 Claude 格畫出來')
    await click(document.querySelector('.quota-bars-open')!)
    const pop = document.querySelector<HTMLElement>('.quota-pop')!
    const section = pop.querySelector<HTMLElement>('[data-k="quota-pop-api"]')
    assert.ok(section, '選單有 API 餘額區')
    assert.ok(section!.querySelector('[data-k="quota-balance-row"]'), 'Claude 一列')
    assert.ok(section!.querySelector('[data-k="openai-balance-row"]'), 'OpenAI 一列')
    await click(section!.querySelector('[data-k="openai-balance-row"]')!)
    assert.ok(document.querySelector('[data-k="openai-balance-dialog"]'), '點了開 OpenAI 校正視窗')
  })
})

it('m4p 有額度格：選單也有 API 餘額區', async () => {
  resetStoreForTest()
  seedRemoteClaudeGauge('m4p')
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => document.querySelector('.quota-bars-open') !== null, 'm4p Claude 格畫出來')
    await click(document.querySelector('.quota-bars-open')!)
    const pop = document.querySelector<HTMLElement>('.quota-pop')!
    assert.ok(pop.querySelector('[data-k="quota-pop-api"] [data-k="quota-balance-row"]'))
    assert.ok(pop.querySelector('[data-k="quota-pop-api"] [data-k="openai-balance-row"]'))
  })
})

it('手機 setOnly：選單只顯示一格額度，API 餘額區仍顯示', async () => {
  resetStoreForTest()
  seedLocalClaudeGauge()
  const origMatch = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  try {
    await withBalances(claudeAnchored, openaiAnchored, async () => {
      await mount(<QuotaStrip />)
      await until(() => document.querySelector('.quota-bars-open') !== null, '手機上的 Claude 格畫出來')
      await click(document.querySelector('.quota-bars-open')!)
      const pop = document.querySelector<HTMLElement>('.quota-pop')!
      assert.ok(pop.querySelector('[data-k="quota-pop-api"] [data-k="quota-balance-row"]'), '手機 setOnly 仍有 API 餘額')
    })
  } finally {
    window.matchMedia = origMatch
  }
})

it('未設定／未啟用的那一列不畫：OpenAI 未啟用時選單只剩 Claude 列', async () => {
  resetStoreForTest()
  seedLocalClaudeGauge()
  await withBalances(claudeAnchored, { enabled: false, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-bars-open') !== null, '本機 Claude 格畫出來')
    await click(document.querySelector('.quota-bars-open')!)
    const section = document.querySelector<HTMLElement>('[data-k="quota-pop-api"]')!
    assert.ok(section.querySelector('[data-k="quota-balance-row"]'))
    assert.equal(section.querySelector('[data-k="openai-balance-row"]'), null)
  })
})
