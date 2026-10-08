/** `usePendingQuestion`：分頁在背景時不讀 transcript 的那一題，回前景立刻補一次（#909）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { usePendingQuestion } from './usePendingQuestion'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

/** transport 在真的 fetch 之前還有幾個 await：多讓幾輪 microtask。 */
const flush = () =>
  act(async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve()
  })

function Probe({ botId }: { botId: string }) {
  usePendingQuestion(botId)
  return null
}

test('背景時不打 pending-question，回前景立刻補一次', async () => {
  resetStoreForTest()
  useStore.setState({ bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as Bot] })
  let reads = 0
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    if (String(input).includes('pending-question')) reads += 1
    return new Response(JSON.stringify({ questions: null }), { status: 200 })
  }) as unknown as typeof fetch
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    await mount(<Probe botId="b1" />)
    await flush()
    assert.equal(reads, 1, '掛上去先讀一次')

    setVisibility('hidden')
    await act(async () => timers.clock.advance(5000))
    await flush()
    assert.equal(reads, 1, '背景：5 秒一輪也不打 API')

    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await flush()
    assert.equal(reads, 2, '回前景：立刻補一次')
  } finally {
    timers.restore()
    setVisibility('visible')
    globalThis.fetch = realFetch
  }
})
