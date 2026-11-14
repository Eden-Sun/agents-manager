import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act } from 'react'
import { mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { ClaudeApiBotLimits } from './ClaudeApiBotLimits'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

test('ClaudeApiBotLimits: 檢視總上限與每回合上限，可修改總上限與每回合時限', async () => {
  const puts: unknown[] = []
  let capState = {
    cap_usd: 20,
    since: '2026-11-14T00:00:00Z',
    spent_usd: 5.5,
    turn_timeout_secs: 1800,
    max_api_calls_per_turn: 20,
  }
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/cap') {
      if (method === 'PUT') {
        puts.push(body)
        const b = body as Record<string, unknown>
        if (b.cap_usd !== undefined) {
          capState.cap_usd = b.cap_usd as number
        }
        if (b.turn_timeout_secs !== undefined) {
          capState.turn_timeout_secs = b.turn_timeout_secs as number
        }
        if (b.max_api_calls_per_turn !== undefined) {
          capState.max_api_calls_per_turn = b.max_api_calls_per_turn as number
        }
      }
      return { ...capState }
    }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request

  try {
    useStore.setState({
      bots: [
        {
          id: 'b-api-1',
          name: 'capi-bot',
          kind: 'claude-api',
          project_id: 'p1',
          created_at: 0,
          model: 'claude-haiku-5-5',
          effort: 'low',
          fast: false,
        } as any,
      ],
    })

    const root = await mount(<ClaudeApiBotLimits />)
    await settle()

    // 驗證標題與初始顯示
    assert.match(root.textContent ?? '', /Claude API Bot（用量與上限）/)
    assert.match(root.textContent ?? '', /已花 \$5\.50，上限 \$20\.00/)
    assert.match(root.textContent ?? '', /每回合時間：1800 秒（1–3600） · API 呼叫：20 次（1–64）/)
    assert.match(root.textContent ?? '', /capi-bot/)

    // 編輯總上限
    const editCapBtn = root.querySelectorAll<HTMLButtonElement>('.claude-api-bot-limits-btn')[0]
    await act(async () => {
      editCapBtn.click()
    })
    await settle()

    const capInput = root.querySelector<HTMLInputElement>('#bot-cap-input')!
    assert.ok(capInput, '總上限輸入框存在')
    assert.equal(capInput.value, '20')

    const setVal = (el: HTMLInputElement, v: string) => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(el, v)
      el.dispatchEvent(new Event('input', { bubbles: true }))
      el.dispatchEvent(new Event('change', { bubbles: true }))
    }

    await act(async () => {
      setVal(capInput, '35')
      root.querySelector<HTMLFormElement>('[data-testid=cap-edit-form]')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    })
    await settle()

    assert.equal((puts[0] as any).cap_usd, 35)
    assert.match(root.textContent ?? '', /上限 \$35\.00/)

    // 編輯回合上限
    const editLimitsBtn = root.querySelector<HTMLButtonElement>('.claude-api-bot-limits-section:nth-of-type(2) .claude-api-bot-limits-btn')!
    await act(async () => {
      editLimitsBtn.click()
    })
    await settle()

    const timeoutInput = root.querySelector<HTMLInputElement>('#bot-turn-timeout-input')!
    const callsInput = root.querySelector<HTMLInputElement>('#bot-turn-calls-input')!
    assert.ok(timeoutInput)
    assert.ok(callsInput)

    await act(async () => {
      setVal(timeoutInput, '2400')
      setVal(callsInput, '30')
      root.querySelector<HTMLFormElement>('[data-testid=turn-limits-form]')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    })
    await settle()

    assert.deepEqual(puts[1], { turn_timeout_secs: 2400, max_api_calls_per_turn: 30 })
    assert.match(root.textContent ?? '', /每回合時間：2400 秒（1–3600） · API 呼叫：30 次（1–64）/)
  } finally {
    rawTransport.request = orig
  }
})

test('ClaudeApiBotLimits: 指定 botId 時顯示該 bot 的累計花費與快取統計', async () => {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/claude-api/cap') {
      return { cap_usd: null, since: null, spent_usd: 12.3, turn_timeout_secs: 1800, max_api_calls_per_turn: 20 }
    }
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 15, recorded: 12, reserved: 2, unknown: 0, failed: 1 },
        cost_microusd: { recorded: 12000000, reserved: 300000, unknown: 0 },
        tokens: { input: 50000, output: 8000, cache_read: 40000, cache_creation: 10000 },
        cache_hit_rate: 0.8,
        last: { speed: 'standard' },
        turns: [],
      }
    }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request

  try {
    const root = await mount(<ClaudeApiBotLimits botId="b-api-1" />)
    await settle()

    const stats = root.querySelector('[data-testid=bot-usage-stats]')
    assert.ok(stats, 'bot-usage-stats 存在')
    assert.match(stats.textContent ?? '', /本 Bot 累計花費：.*\$12\.30/)
    assert.match(stats.textContent ?? '', /快取命中率：.*80%/)
    assert.match(stats.textContent ?? '', /請求次數：.*15/)
  } finally {
    rawTransport.request = orig
  }
})
