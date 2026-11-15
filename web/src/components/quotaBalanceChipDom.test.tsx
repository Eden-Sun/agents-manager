/**
 * 全域額度列的 Anthropic API 餘額（BalanceQuotaChip）：估算／Console 實際、tooltip 放不確定與更新時間、手機短格式、
 * 沒設定不畫、點下去到第一顆 claude-api bot。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { resetClaudeApiBalanceForTest } from '../hooks/useClaudeApiBalance'
import { BalanceQuotaChip } from './BalanceQuotaChip'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetClaudeApiBalanceForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const estimate = {
  configured: true,
  anchor: { usd: 98.14, at: '2026-11-02T05:52:10.209Z' },
  spent_usd: 74.85,
  remaining_usd: 23.29,
  estimate: true,
  uncertain_usd: 19.59,
  remaining_floor_usd: 3.7,
  report_through: '2026-11-15T00:00:00.000Z',
  local_since: '2026-11-15T00:00:00.000Z',
  as_of: '2026-11-15T09:30:00.000Z',
}

async function withBalance<T>(balance: unknown, fn: () => Promise<T>): Promise<T> {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/balance') return balance
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    return await fn()
  } finally {
    rawTransport.request = orig
  }
}

const settle = () => act(async () => { await new Promise((r) => setTimeout(r, 30)) })
const chip = () => document.querySelector<HTMLElement>('[data-k="quota-balance"]')

test('估算：顯示 API ≈$23.29；tooltip 標示估算、已確定花費、不確定與最壞餘額、更新時間', async () => {
  useStore.setState({ bots: [] } as never)
  await withBalance(estimate, async () => {
    await mount(<BalanceQuotaChip />)
    await settle()
    assert.ok(chip(), '有設定錨點就畫')
    assert.equal(chip()!.textContent, 'API≈$23.29')
    const tip = chip()!.getAttribute('title') ?? ''
    assert.match(tip, /估算/)
    assert.match(tip, /已確定花費/)
    assert.match(tip, /最多 \$19\.59.*最壞情況餘額 \$3\.70/)
    assert.match(tip, /更新於 \d\d:\d\d/)
    assert.equal(chip()!.tagName, 'SPAN', '沒有 claude-api bot 時不可點')
  })
})

test('Console 讀得到：顯示實際值（沒有 ≈）', async () => {
  useStore.setState({ bots: [] } as never)
  await withBalance({ ...estimate, console: { status: 'ok', purchased_usd: 20, promotional_usd: 1.5, total_usd: 21.5, promotions: [], fetched_at: '2026-11-15T09:00:00.000Z' } }, async () => {
    await mount(<BalanceQuotaChip />)
    await settle()
    assert.equal(chip()!.textContent, 'API$21.50')
    assert.match(chip()!.getAttribute('title') ?? '', /Console 實際值/)
  })
})

test('沒設定（沒有錨點、讀不到 Console）：整格不畫', async () => {
  await withBalance({ configured: false }, async () => {
    await mount(<BalanceQuotaChip />)
    await settle()
    assert.equal(chip(), null)
  })
})

test('點一下切到第一顆 claude-api bot', async () => {
  useStore.setState({
    bots: [
      { id: 'cli-1', name: 'cli', kind: 'claude', project_id: 'p' },
      { id: 'api-1', name: 'api', kind: 'claude-api', project_id: 'p' },
    ],
    selectedBotId: 'cli-1',
  } as never)
  await withBalance(estimate, async () => {
    await mount(<BalanceQuotaChip />)
    await settle()
    assert.equal(chip()!.tagName, 'BUTTON')
    await click(chip()!)
    assert.equal(useStore.getState().selectedBotId, 'api-1')
  })
})

test('手機：縮成短格式 $23.3', async () => {
  useStore.setState({ bots: [] } as never)
  const origMatch = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  try {
    await withBalance(estimate, async () => {
      await mount(<BalanceQuotaChip />)
      await settle()
      assert.equal(chip()!.textContent, '$23.3')
    })
  } finally {
    window.matchMedia = origMatch
  }
})
