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

test('ApiBotUsageBar 的 speed 標成「上一次 speed」，tooltip 說明不是目前設定', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 1000, reserved: 0, unknown: 0 },
        tokens: { input: 1, output: 1, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null,
        last: { speed: 'fast', at: null, input_tokens: 1, output_tokens: 1 },
        turns: [],
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') return { cap_usd: null, since: null, spent_usd: 0 }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 50)) })
    const item = document.querySelector('[data-k="speed"]')!
    assert.ok(item, 'speed 項目要在')
    assert.match(item.querySelector('.sl-k')!.textContent ?? '', /上一次/)
    assert.equal(item.querySelector('.sl-v')!.textContent, 'fast')
    assert.match(item.getAttribute('title') ?? '', /不是目前設定/)
  } finally {
    rawTransport.request = orig
  }
})

test('ApiBotUsageBar 的「回合」晶片：顯示每回合時間／呼叫上限，可改並送出，被 daemon 拒絕時留在表單顯示原因', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })
  const orig = rawTransport.request.bind(rawTransport)
  let limits = { turn_timeout_secs: 1800, max_api_calls_per_turn: 20 }
  const puts: unknown[] = []
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 1000, reserved: 0, unknown: 0 },
        tokens: { input: 1, output: 1, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null,
        last: null,
        turns: [],
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') {
      if (method === 'PUT') {
        puts.push(body)
        const b = body as { turn_timeout_secs: number; max_api_calls_per_turn: number }
        if (b.turn_timeout_secs < 60) throw new Error('turn_timeout_below_command_timeout')
        limits = b
      }
      return { cap_usd: null, since: null, spent_usd: 0, ...limits }
    }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  const settleUi = () => act(async () => { await new Promise((r) => setTimeout(r, 50)) })
  const type = async (el: HTMLInputElement, v: string) => {
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value')!.set!
      set.call(el, v)
      el.dispatchEvent(new Event('input', { bubbles: true }))
      el.dispatchEvent(new Event('change', { bubbles: true }))
    })
  }
  const submit = () => act(async () => {
    document.querySelector('[data-k="turn-limits-form"]')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await new Promise((r) => setTimeout(r, 50))
  })
  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)
    await settleUi()
    const chip = document.querySelector<HTMLButtonElement>('[data-k="turn-limits"]')!
    assert.ok(chip, '回合晶片要在')
    assert.equal(chip.querySelector('.sl-v')!.textContent, '30m / 20')
    await act(async () => { chip.click() })
    const form = document.querySelector('[data-k="turn-limits-form"]')!
    assert.equal(form.querySelector<HTMLInputElement>('#turn-timeout-input')!.value, '1800')
    // 超出範圍：不送出。
    await type(form.querySelector<HTMLInputElement>('#turn-timeout-input')!, '5000')
    await submit()
    assert.equal(puts.length, 0)
    assert.ok(document.querySelector('[role="alert"]'), '顯示範圍提示')
    // daemon 拒絕：留在表單、顯示原因。
    await type(form.querySelector<HTMLInputElement>('#turn-timeout-input')!, '30')
    await submit()
    await settleUi()
    assert.equal(puts.length, 1)
    assert.match(document.querySelector('[role="alert"]')?.textContent ?? '', /turn_timeout_below_command_timeout/)
    // 成功：只送每回合上限，不帶 cap_usd；晶片更新。
    await type(document.querySelector<HTMLInputElement>('#turn-timeout-input')!, '2400')
    await type(document.querySelector<HTMLInputElement>('#turn-calls-input')!, '30')
    await submit()
    await settleUi()
    assert.deepEqual(puts[1], { turn_timeout_secs: 2400, max_api_calls_per_turn: 30 })
    assert.equal(document.querySelector('[data-k="turn-limits"] .sl-v')!.textContent, '40m / 30')
  } finally {
    rawTransport.request = orig
  }
})
