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
    assert.equal(chip.querySelector('.sl-v')!.textContent, '30m · 20 calls')
    await act(async () => { chip.click() })
    const form = document.querySelector('[data-k="turn-limits-form"]')!
    assert.equal(form.querySelector<HTMLInputElement>('#turn-timeout-input')!.value, '1800')
    assert.match(form.textContent ?? '', /回合時間（秒，1–3600）/)
    assert.match(form.textContent ?? '', /呼叫次數（1–64）/)
    assert.match(form.textContent ?? '', /下一回合起生效/)
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
    assert.equal(document.querySelector('[data-k="turn-limits"] .sl-v')!.textContent, '40m · 30 calls')
  } finally {
    rawTransport.request = orig
  }
})

test('ApiBotUsageBar 在手機寬度（PHONE_QUERY）時收合次要晶片，提供展開/收合按鈕並可編輯上限', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })
  const origMatch = window.matchMedia
  window.matchMedia = ((q: string) => ({
    matches: q.includes('max-width: 640px'),
    media: q,
    addEventListener() {},
    removeEventListener() {},
  })) as unknown as typeof window.matchMedia

  const origReq = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 5, recorded: 5, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 2000000, reserved: 0, unknown: 0 },
        tokens: { input: 100, output: 50, cache_read: 20, cache_creation: 10 },
        cache_hit_rate: 0.5,
        last: { speed: 'fast' },
        turns: [],
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') {
      return { cap_usd: 15, since: '2026-11-14T00:00:00Z', spent_usd: 2.0, turn_timeout_secs: 1800, max_api_calls_per_turn: 20 }
    }
    return origReq(method as any, path, body)
  }) as typeof rawTransport.request

  const settleUi = () => act(async () => { await new Promise((r) => setTimeout(r, 50)) })

  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)
    await settleUi()

    // 手機預設：有 Cap、Turn、Spent 與展開鈕 ▾；Cache、Speed、Calls 收合
    assert.ok(document.querySelector('[data-k="cap"]'), 'Cap 晶片要在')
    assert.ok(document.querySelector('[data-k="turn-limits"]'), '回合晶片要在')
    assert.ok(document.querySelector('[data-k="cost"]'), '累計花費要在')
    assert.equal(document.querySelector('[data-k="cache"]'), null, '快取晶片預設收合')
    assert.equal(document.querySelector('[data-k="speed"]'), null, 'speed 晶片預設收合')

    const expandBtn = document.querySelector<HTMLButtonElement>('.api-usage-expand-btn')!
    assert.ok(expandBtn, '手機展開按鈕要在')
    assert.equal(expandBtn.textContent, '▾')

    // 點擊展開
    await act(async () => { expandBtn.click() })
    await settleUi()

    assert.ok(document.querySelector('[data-k="cache"]'), '展開後快取晶片要在')
    assert.ok(document.querySelector('[data-k="speed"]'), '展開後 speed 晶片要在')
    assert.ok(document.querySelector('[data-k="calls"]'), '展開後 requests 晶片要在')
    assert.equal(expandBtn.textContent, '▴')

    // 點擊收合
    await act(async () => { expandBtn.click() })
    await settleUi()
    assert.equal(document.querySelector('[data-k="cache"]'), null, '收合後快取晶片隱藏')
  } finally {
    window.matchMedia = origMatch
    rawTransport.request = origReq
  }
})

test('ApiBotUsageBar 顯示目前 context 與壓縮門檻（例：148k / 壓縮 150k），沒有資料時不顯示', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })
  const orig = rawTransport.request.bind(rawTransport)
  let context: unknown = { tokens: 148_012, compact_at: 150_000 }
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 1000, reserved: 0, unknown: 0 },
        tokens: { input: 1, output: 1, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null, last: null, turns: [], context,
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') return { cap_usd: null, since: null, spent_usd: 0 }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 50)) })
    const item = document.querySelector('[data-k="context"]')!
    assert.ok(item, 'context 項目要在')
    assert.match(item.querySelector('.sl-v')!.textContent ?? '', /^148k \/ .*150k$/)
    assert.ok(item.classList.contains('warn'), '接近門檻（≥90%）要標警示')
  } finally {
    rawTransport.request = orig
  }
})

test('ApiBotUsageBar 的快取倒數：熱剩 m:ss、回合中「熱」、過期「已冷」', async () => {
  const { apiCacheChip, clockText } = await import('../lib/composerCost')
  assert.equal(clockText(222), '3:42')
  assert.equal(clockText(3725), '1:02:05')
  const base = { last_request_at: '2026-11-15T03:00:00.000Z', ttl_secs: 300, expires_at: '2026-11-15T03:05:00.000Z', working: false }
  const at = (iso: string) => Date.parse(iso)
  const hot = apiCacheChip(base, at('2026-11-15T03:01:18.000Z'), 148_000, 'claude-opus-5-5', false)!
  assert.match(hot.label, /3:42/)
  assert.equal(hot.level, 'low') // 剩不到 5 分鐘＝紅
  assert.equal(apiCacheChip({ ...base, working: true }, at('2026-11-15T03:30:00.000Z'), null, null, false)!.level, 'hot')
  const cold = apiCacheChip(base, at('2026-11-15T03:06:00.000Z'), 100_000, 'claude-opus-5-5', false)!
  assert.equal(cold.level, 'cold')
  assert.match(cold.label, /\$0\.50/, '10 萬 token × $5/M 的 5 分鐘寫入價')
  assert.equal(apiCacheChip({ ...base, last_request_at: null, expires_at: null, ttl_secs: null }, Date.now(), null, null, false), null)
  // 1 小時 TTL 用 1h 寫入單價；未知模型只講已冷。
  assert.match(apiCacheChip({ ...base, ttl_secs: 3600, expires_at: '2026-11-15T04:00:00.000Z' }, at('2026-11-15T05:00:00.000Z'), 100_000, 'claude-opus-5-5', false)!.label, /\$0\.80/)
  assert.equal(apiCacheChip(base, at('2026-11-15T04:00:00.000Z'), 100_000, 'mystery', false)!.label, '已冷')
})

test('ApiBotUsageBar 畫出快取倒數項目', async () => {
  useStore.setState({ turns: { 'bot-1': {} } })
  const orig = rawTransport.request.bind(rawTransport)
  const expires = new Date(Date.now() + 222_000).toISOString()
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 1000, reserved: 0, unknown: 0 },
        tokens: { input: 1, output: 1, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null, last: null, turns: [], context: { tokens: 1000, compact_at: 150000 },
        cache: { last_request_at: new Date(Date.now() - 78_000).toISOString(), ttl_secs: 300, expires_at: expires, working: false },
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') return { cap_usd: null, since: null, spent_usd: 0 }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<ApiBotUsageBar botId="bot-1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 50)) })
    const item = document.querySelector('[data-k="cache-countdown"]')!
    assert.ok(item, '快取倒數項目要在')
    assert.match(item.querySelector('.sl-v')!.textContent ?? '', /3:4\d|3:3\d/)
  } finally {
    rawTransport.request = orig
  }
})

test('ApiBotUsageBar 畫出卡住徽章與原因', async () => {
  useStore.setState({ turns: { 'bot-stuck': {} } })
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 0, reserved: 0, unknown: 1, failed: 0 },
        cost_microusd: { recorded: 0, reserved: 0, unknown: 1000 },
        tokens: { input: 0, output: 0, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null, last: null, turns: [], context: { tokens: null, compact_at: null },
        cache: { last_request_at: null, ttl_secs: null, expires_at: null, working: false },
        stuck: { reason: 'checkpoint_pending_after_owner_exit', message: '回合結束但仍有未結算標記' },
      }
    }
    if (path === '/claude-api/balance') return { configured: false }
    if (path === '/claude-api/cap') return { cap_usd: null, since: null, spent_usd: 0 }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  try {
    await mount(<ApiBotUsageBar botId="bot-stuck" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 50)) })
    const stuckChip = document.querySelector('[data-k="stuck"]')
    assert.ok(stuckChip, '卡住徽章要在')
    assert.equal(stuckChip.querySelector('.sl-k')?.textContent, '卡住')
    assert.equal(stuckChip.querySelector('.sl-v')?.textContent, 'checkpoint_pending_after_owner_exit')
  } finally {
    rawTransport.request = orig
  }
})
