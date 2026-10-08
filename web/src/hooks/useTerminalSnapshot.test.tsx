/** `useTerminalSnapshot`：分頁在背景（`visibilityState === 'hidden'`）時不輪詢，回前景立刻補一次（#909）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { resetStoreForTest, useStore } from '../store/store'
import type { TerminalSnapshot } from '../api/types'
import { useTerminalSnapshot } from './useTerminalSnapshot'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

function Probe({ botId }: { botId: string }) {
  useTerminalSnapshot(botId, { intervalMs: 1000 })
  return null
}

test('背景時不打 /terminal，回前景立刻補一次', async () => {
  resetStoreForTest()
  let reads = 0
  useStore.setState({
    readTerminal: async () => {
      reads += 1
      return { text: '', revision: null, truncated: false, source: 'visible', pane_id: null, columns: null, rows: null } as TerminalSnapshot
    },
  })
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    await mount(<Probe botId="b1" />)
    assert.equal(reads, 1, '掛上去先讀一次')

    setVisibility('hidden')
    await act(async () => timers.clock.advance(5000))
    assert.equal(reads, 1, '背景：輪詢照排但不打 API')

    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    assert.equal(reads, 2, '回前景：立刻補一次')

    await act(async () => timers.clock.advance(1000))
    assert.equal(reads, 3, '之後照常每 intervalMs 一次，不會變成兩條輪詢鏈')
  } finally {
    timers.restore()
    setVisibility('visible')
  }
})
