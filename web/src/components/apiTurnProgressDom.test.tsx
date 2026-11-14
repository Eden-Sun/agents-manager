/** claude-api 串流中：live bubble 的進度列顯示「≈ $x 目前為止」，結算後（無估價）回到「已花（含預留）」。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiTurnProgress } from './ApiTurnCost'
import { rawTransport } from '../api'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const usage = {
  attempts: { total: 1, recorded: 1, reserved: 1, unknown: 0, failed: 0 },
  cost_microusd: { recorded: 20_000, reserved: 90_000, unknown: 0 },
  tokens: { input: 0, output: 0, cache_read: 0, cache_creation: 0 },
  cache_hit_rate: null,
  last: null,
  turns: [{ turn_id: 't1', requests: 2, cost_usd: 0.02, reserved_usd: 0.09, in_flight: true, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0 }],
}

test('串流估價存在時顯示「≈ $x 目前為止」（已結算＋這次請求的估價），沒有時顯示含預留的舊字樣', async () => {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) return usage
    return orig(method as any, path, body)
  }) as typeof rawTransport.request
  try {
    useStore.setState({
      turns: { b1: { t1: { id: 't1', status: 'in_flight' } as never } },
      liveReply: { b1: { turnId: 't1', text: '', activity: '', alert: '', revision: 3, estCostMicrousd: 12_345 } },
    })
    await mount(<ApiTurnProgress botId="b1" />)
    const text = () => document.querySelector('.msg-cost')?.textContent ?? ''
    // 空文字（只在思考）也要顯示估價；0.02 已結算 + 0.012345 估價 = 0.032。
    await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
    assert.match(text(), /≈ \$0\.032/)
    assert.doesNotMatch(text(), /預留/)

    // 估價消失（回合結算、或換成別的 turn 的幀）：回到已花（含預留）。
    await act(async () => {
      useStore.setState({ liveReply: { b1: { turnId: 'other', text: '', activity: '', alert: '', revision: 1, estCostMicrousd: 5 } } })
    })
    assert.match(text(), /已花 \$0\.110（含預留 \$0\.090）/)
  } finally {
    rawTransport.request = orig
  }
})
