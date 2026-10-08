/** `useBlockedPrefetch`：分頁在背景時整個略過（不只擋預載），回前景立刻補一次（#909）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { resetStoreForTest, useStore } from '../store/store'
import { resetBlockedPrefetch } from '../lib/blockedPrefetch'
import type { Run, TerminalSnapshot } from '../api/types'
import { useBlockedPrefetch } from './useBlockedPrefetch'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetBlockedPrefetch()
  resetStoreForTest()
  teardownDom()
})

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

function Probe() {
  useBlockedPrefetch()
  return null
}

test('背景時不讀 blocked bot 的終端，回前景立刻補一次', async () => {
  resetStoreForTest()
  let reads = 0
  useStore.setState({
    runs: { b1: { agent_status: 'blocked' } as unknown as Run },
    readTerminal: async () => {
      reads += 1
      return { text: '', revision: null, truncated: false, source: 'visible', pane_id: null, columns: null, rows: null } as TerminalSnapshot
    },
  })
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    await mount(<Probe />)
    assert.equal(reads, 1, 'blocked 一出現就讀一次')

    setVisibility('hidden')
    await act(async () => timers.clock.advance(8000))
    assert.equal(reads, 1, '背景：4 秒一輪也不讀')

    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    assert.equal(reads, 2, '回前景：立刻補一次')
  } finally {
    timers.restore()
    setVisibility('visible')
    resetBlockedPrefetch()
  }
})
