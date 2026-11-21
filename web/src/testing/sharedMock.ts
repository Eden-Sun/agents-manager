import { after, before } from 'node:test'
import { MockTransport } from '../api/mock'
import { manualMockClock, useManualMockClock } from '../api/mockClock'
import { POLL_TICK_MS, setPollTick } from './domHarness'

export const sharedMock = new MockTransport()

export function closeLeakedShells(): number {
  const internals = sharedMock as unknown as { shells?: Map<string, unknown>; loginPanes?: Map<string, unknown> }
  if (!(internals.shells instanceof Map) || !(internals.loginPanes instanceof Map)) throw new Error('MockTransport 的 shells／loginPanes 欄位不見了，更新 closeLeakedShells')
  const n = internals.shells.size
  internals.shells.clear()
  internals.loginPanes.clear()
  return n
}

export function advanceMockTime(ms: number): Promise<void> {
  return manualMockClock.advance(ms)
}

export function virtualMockTime(): void {
  before(() => {
    useManualMockClock(true)
    setPollTick(() => manualMockClock.advance(POLL_TICK_MS))
  })
  after(() => {
    setPollTick(null)
    useManualMockClock(false)
    closeLeakedShells()
  })
}
