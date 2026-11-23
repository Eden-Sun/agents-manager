import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { quotaKey } from '../api/types'
import { resetClaudeApiBalanceForTest } from '../hooks/useClaudeApiBalance'
import { reloadOpenAiApiBalance, resetOpenAiApiBalanceForTest } from '../hooks/useOpenAiApiBalance'
import { QuotaStrip } from './QuotaStrip'

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

const rect = (width: number) => ({ x: 0, y: 0, left: 0, top: 0, right: width, bottom: 0, width, height: 0, toJSON() { return this } }) as DOMRect

function squeezeLayout(): () => void {
  const proto = HTMLElement.prototype as unknown as Record<string, unknown>
  const names = ['getBoundingClientRect', 'getClientRects', 'clientWidth'] as const
  const saved = names.map((n) => [n, Object.getOwnPropertyDescriptor(proto, n)] as const)
  Object.defineProperty(proto, 'getBoundingClientRect', { configurable: true, value: () => rect(300) })
  Object.defineProperty(proto, 'getClientRects', { configurable: true, value: () => [rect(300)] })
  Object.defineProperty(proto, 'clientWidth', { configurable: true, get: () => 100 })
  return () => {
    for (const [n, d] of saved) {
      if (d) Object.defineProperty(proto, n, d)
      else delete proto[n]
    }
  }
}

before(() => {
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
  remaining_usd: 16.84,
  estimate: true,
  uncertain_usd: 0,
  remaining_floor_usd: 16.84,
  as_of: '2026-11-19T09:30:00.000Z',
}
const openaiAnchored = {
  enabled: true,
  configured: true,
  anchor: { usd: 100, at: '2026-11-10T00:00:00.000Z' },
  spent_usd: 17,
  remaining_usd: 83,
  uncertain_usd: 0,
  remaining_floor_usd: 83,
  as_of: '2026-11-19T01:00:00.000Z',
}

async function withBalances<T>(claude: unknown, openai: unknown, fn: () => Promise<T>): Promise<T> {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/balance') return claude
    if (path === '/openai-api/balance') return openai
    if (path === '/openai-api/balance-anchor' && method === 'PUT') return { ok: true }
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    return await fn()
  } finally {
    rawTransport.request = orig
  }
}

const settle = () => new Promise((r) => setTimeout(r, 30))
const group = () => document.querySelector<HTMLElement>('[data-k="quota-api-group"]')
const claudeChip = () => document.querySelector<HTMLElement>('[data-k="quota-balance"]')
const openaiChip = () => document.querySelector<HTMLElement>('[data-k="openai-balance"]')

function seedClaudeGauge(host: string) {
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

it('本機有量表：群組在 .quota-strip 裡、不在 .quota-open 裡（收合不會裁到它）', async () => {
  resetStoreForTest()
  seedClaudeGauge('local')
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip />)
    await until(() => group() !== null && document.querySelector('.quota-hp') !== null, '群組與量表都畫出來')
    const g = group()!
    assert.equal(g.closest('.quota-open'), null, '群組不在會被收合裁切的 .quota-open 裡')
    assert.equal(g.parentElement?.classList.contains('quota-strip'), true, '群組是 .quota-strip 的子項（寬度由 fit 量測扣掉）')
  })
})

it('空間很窄、量表收合時：群組仍在 DOM，且不在收合的那一列裡', async () => {
  resetStoreForTest()
  seedClaudeGauge('local')
  const restore = squeezeLayout()
  try {
    await withBalances(claudeAnchored, openaiAnchored, async () => {
      await mount(<QuotaStrip />)
      await until(() => document.querySelector('.quota-open.collapsed') !== null, '窄寬度時量表收合')
      await settle()
      const g = group()
      assert.ok(g, '收合時群組仍在')
      assert.equal(g!.closest('.quota-open.collapsed'), null, '裁掉的只能是量表，不是餘額')
      assert.ok(openaiChip() && claudeChip(), '兩家都在群組裡')
    })
  } finally {
    restore()
  }
})

it('m4p 沒有任何額度格：仍畫出群組與兩家餘額，不畫量表', async () => {
  resetStoreForTest()
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => group() !== null, 'm4p 的餘額群組畫出來')
    assert.equal(document.querySelectorAll('.quota-hp').length, 0, '沒有額度格就不畫量表')
    assert.ok(claudeChip() && openaiChip(), '兩家都在')
  })
})

it('m4p 有額度格：群組也畫出來', async () => {
  resetStoreForTest()
  seedClaudeGauge('m4p')
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => group() !== null && document.querySelector('.quota-hp') !== null, 'm4p 群組與量表畫出來')
    assert.ok(claudeChip() && openaiChip())
  })
})

it('兩家都不可見：整條額度列不畫（沒有群組）', async () => {
  resetStoreForTest()
  await withBalances({ configured: false }, { enabled: false, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await settle()
    assert.equal(document.querySelector('.quota-strip'), null)
    assert.equal(group(), null)
  })
})

it('只有 Claude 有值：群組只一行（沒有 OpenAI 晶片）', async () => {
  resetStoreForTest()
  await withBalances(claudeAnchored, { enabled: false, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => group() !== null, '群組畫出來')
    assert.ok(claudeChip())
    assert.equal(openaiChip(), null, 'OpenAI 未啟用不畫')
    assert.equal(group()!.children.length, 1)
  })
})

it('兩家都有值：群組兩行（Claude 在上、OpenAI 在下）', async () => {
  resetStoreForTest()
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => openaiChip() !== null, 'OpenAI 晶片畫出來')
    const kids = [...group()!.children]
    assert.equal(kids.length, 2)
    assert.equal(kids[0]!.getAttribute('data-k'), 'quota-balance')
    assert.equal(kids[1]!.getAttribute('data-k'), 'openai-balance')
  })
})

it('點擊各自開對應的校正視窗', async () => {
  resetStoreForTest()
  await withBalances(claudeAnchored, openaiAnchored, async () => {
    await mount(<QuotaStrip host="m4p" />)
    await until(() => openaiChip() !== null, 'OpenAI 晶片畫出來')
    await click(openaiChip()!)
    assert.ok(document.querySelector('[data-k="openai-balance-dialog"]'), 'OpenAI 視窗')
    await click(claudeChip()!)
    assert.ok(document.querySelector('[data-k="quota-balance-dialog"]'), 'Claude 視窗')
  })
})

it('餘額群組在初次量測後才出現：額度條重新量測並收合，不被群組蓋住', async () => {
  resetStoreForTest()
  seedClaudeGauge('local')
  const proto = HTMLElement.prototype as unknown as Record<string, unknown>
  const savedRect = Object.getOwnPropertyDescriptor(proto, 'getBoundingClientRect')
  const savedWidth = Object.getOwnPropertyDescriptor(proto, 'clientWidth')
  Object.defineProperty(proto, 'getBoundingClientRect', {
    configurable: true,
    value(this: HTMLElement) {
      const w = this.dataset?.k === 'quota-api-group' ? 500 : this.classList?.contains('quota-hp') ? 300 : 0
      return rect(w)
    },
  })
  Object.defineProperty(proto, 'clientWidth', { configurable: true, get: () => 400 })
  const openai: { current: unknown } = { current: { enabled: false, configured: false } }
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/openai-api/balance') return openai.current
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-open') !== null, '額度條畫出來')
    await settle()
    assert.equal(document.querySelector('.quota-open.collapsed'), null, '沒有群組時寬度足夠，不收合')
    openai.current = openaiAnchored
    await reloadOpenAiApiBalance()
    await until(() => document.querySelector('.quota-open.collapsed') !== null, '群組出現後額度條收合')
    assert.ok(group() !== null, '群組在 DOM')
  } finally {
    rawTransport.request = orig
    for (const [n, d] of [['getBoundingClientRect', savedRect], ['clientWidth', savedWidth]] as const) {
      if (d) Object.defineProperty(proto, n, d)
      else delete proto[n]
    }
  }
})
