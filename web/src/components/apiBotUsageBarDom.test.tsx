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

test('ApiBotUsageBar 顯示唯讀 Balance 晶片與 Cap 晶片及編輯/清除表單', async () => {
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

  let currentCap: { cap_usd: number | null; since: string | null; spent_usd: number } = {
    cap_usd: null,
    since: null,
    spent_usd: 0,
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
    if (path === '/claude-api/cap') {
      if (method === 'GET') {
        return currentCap
      }
      if (method === 'PUT') {
        const b = body as { cap_usd: number | null }
        currentCap = {
          cap_usd: b.cap_usd,
          since: b.cap_usd !== null ? '2026-11-02T05:00:00Z' : null,
          spent_usd: 0,
        }
        return currentCap
      }
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

    // Balance 晶片唯讀渲染
    const balanceChip = bar.querySelector<HTMLElement>('.api-balance-chip[data-k="balance"]')
    assert.ok(balanceChip, 'balance chip should render when configured')
    assert.ok(balanceChip.classList.contains('readonly'), 'balance chip must be readonly')
    assert.ok(balanceChip.textContent?.includes('≈ $98.50'), 'shows remaining balance')

    // 未設定上限時應顯示「設定上限」
    const capSetupBtn = bar.querySelector<HTMLButtonElement>('.api-balance-chip[data-k="cap-setup"]')
    assert.ok(capSetupBtn, 'cap setup button should render when cap is null')

    // 點擊進行設定上限
    await act(async () => {
      capSetupBtn.click()
    })

    const form = bar.querySelector<HTMLFormElement>('.api-balance-form')
    assert.ok(form, 'form should render in cap editing mode')

    const input = form.querySelector<HTMLInputElement>('#cap-input')
    assert.ok(input, 'cap input should render')

    // 變更數值為 10.00 並送出
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(input), 'value')!.set!
      set.call(input, '10.00')
      input.dispatchEvent(new Event('input', { bubbles: true }))
      input.dispatchEvent(new Event('change', { bubbles: true }))
    })

    await act(async () => {
      form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
      await new Promise((resolve) => setTimeout(resolve, 50))
    })

    // 表單應關閉，並顯示新上限「上限 $0.00 / $10.00」
    const updatedCapChip = bar.querySelector<HTMLButtonElement>('.api-balance-chip[data-k="cap"]')
    assert.ok(updatedCapChip, 'cap chip should be visible after save')
    assert.ok(updatedCapChip.textContent?.includes('$0.00 / $10.00'), 'updated cap should be shown')

    // 再次點擊打開表單並測試清除
    await act(async () => {
      updatedCapChip.click()
    })

    const form2 = bar.querySelector<HTMLFormElement>('.api-balance-form')
    assert.ok(form2, 'form should render again')
    const clearBtn = form2.querySelector<HTMLButtonElement>('.api-balance-btn.clear')
    assert.ok(clearBtn, 'clear button should exist')

    await act(async () => {
      clearBtn.click()
      await new Promise((resolve) => setTimeout(resolve, 50))
    })

    // 清除後應變回「設定上限」
    const revertedSetupBtn = bar.querySelector<HTMLButtonElement>('.api-balance-chip[data-k="cap-setup"]')
    assert.ok(revertedSetupBtn, 'should revert to cap setup button after clear')
  } finally {
    rawTransport.request = origRequest
  }
})
