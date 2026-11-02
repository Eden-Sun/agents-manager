import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiBotUsageBar } from './ApiBotUsageBar'
import { rawTransport } from '../api'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

test('ApiBotUsageBar 顯示 Balance 晶片與點擊編輯表單', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })

  const origRequest = rawTransport.request.bind(rawTransport)

  let currentBalance = {
    configured: true,
    anchor: { usd: 100.0, at: '2026-11-02T00:00:00Z' },
    spent_usd: 1.5,
    remaining_usd: 98.5,
    report_through: '2026-11-01T23:59:59Z',
    local_since: '2026-11-02T00:00:00Z',
  }

  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 5, recorded: 5, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 200_000, reserved: 0, unknown: 0 },
        tokens: { input: 100, output: 50, cache_read: 20, cache_creation: 10 },
        cache_hit_rate: 0.2,
        last: { speed: 'standard', at: null, input_tokens: 100, output_tokens: 50 },
      }
    }
    if (path === '/claude-api/balance') {
      return currentBalance
    }
    if (path === '/claude-api/balance-anchor' && method === 'PUT') {
      const b = body as { usd: number; at?: string }
      currentBalance = {
        ...currentBalance,
        anchor: { usd: b.usd, at: b.at ?? '2026-11-02T04:00:00Z' },
        spent_usd: 0,
        remaining_usd: b.usd,
      }
      return currentBalance
    }
    return origRequest(method as any, path, body)
  }) as typeof rawTransport.request

  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)

    // 等待 useEffect 非同步載入完成
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 50))
    })

    const bar = document.querySelector('[data-testid="api-usage-bar"]')
    assert.ok(bar, 'api-usage-bar should render')

    const balanceChip = bar.querySelector<HTMLButtonElement>('.api-balance-chip')
    assert.ok(balanceChip, 'balance chip should render when configured')
    assert.ok(balanceChip.textContent?.includes('≈ $98.50'), 'shows remaining balance')

    // 點擊進行校準
    await act(async () => {
      balanceChip.click()
    })

    const form = bar.querySelector<HTMLFormElement>('.api-balance-form')
    assert.ok(form, 'form should render in editing mode')

    const input = form.querySelector<HTMLInputElement>('.api-balance-input')
    assert.ok(input, 'input should render')
    assert.equal(input.value, '98.50')

    // 變更數值並送出
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(input), 'value')!.set!
      set.call(input, '95.00')
      input.dispatchEvent(new Event('input', { bubbles: true }))
      input.dispatchEvent(new Event('change', { bubbles: true }))
    })

    await act(async () => {
      form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
      await new Promise((resolve) => setTimeout(resolve, 50))
    })

    // 表單應關閉，並顯示新餘額
    const updatedChip = bar.querySelector<HTMLButtonElement>('.api-balance-chip')
    assert.ok(updatedChip, 'chip should be visible after save')
    assert.ok(updatedChip.textContent?.includes('≈ $95.00'), 'updated balance should be shown')
  } finally {
    rawTransport.request = origRequest
  }
})
