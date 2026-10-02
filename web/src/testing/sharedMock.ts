import { after, before } from 'node:test'
import { MockTransport } from '../api/mock'
import { manualMockClock, useManualMockClock } from '../api/mockClock'
import { setPollTick } from './domHarness'

/**
 * 整個行程共用的一個 `MockTransport`：store 會丟掉 `daemon_seq` 比較小的快照，每個測試檔各建一個等於「daemon 重啟」，
 * store 就不更新了；草稿的 rev 也是同理。用它的測試檔收尾要 `resetStoreForTest()`（store 是模組單例）。
 */
export const sharedMock = new MockTransport()

/** 讓 mock 的虛擬時間走 `ms`（例如 `openSocket` 要等 120ms 才算連上）。 */
export function advanceMockTime(ms: number): Promise<void> {
  return manualMockClock.advance(ms)
}

/**
 * 這個測試檔的 mock 用虛擬時間（`api/mockClock.ts`）：mock 的 timer 不走牆鐘，只在 `until` 輪詢時往前（每輪 250ms）。
 * 沒有它，「mock 回合還在忙」「bot 啟動要 2.5 秒」都是牆鐘上的賭注，高負載下會提前結束。在測試檔最上層呼叫一次。
 */
export function virtualMockTime(): void {
  before(() => {
    useManualMockClock(true)
    setPollTick(() => manualMockClock.advance(250))
  })
  after(() => {
    setPollTick(null)
    useManualMockClock(false)
  })
}
