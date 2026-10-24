import { after, before } from 'node:test'
import { MockTransport } from '../api/mock'
import { manualMockClock, useManualMockClock } from '../api/mockClock'
import { POLL_TICK_MS, setPollTick } from './domHarness'

/**
 * 整個行程共用的一個 `MockTransport`：store 會丟掉 `daemon_seq` 比較小的快照，每個測試檔各建一個等於「daemon 重啟」，
 * store 就不更新了；草稿的 rev 也是同理。用它的測試檔收尾要 `resetStoreForTest()`（store 是模組單例）。
 */
export const sharedMock = new MockTransport()

/**
 * 關掉共用 mock 上所有還開著的主機 shell，回傳關了幾顆。mock 跟 daemon 一樣每台主機最多 8 顆（`too_many_shells`），
 * 而 `sharedMock` 整個行程只有一個：某個測試檔漏關的 shell 會留給後面的檔案。Linux 的檔案順序剛好讓它們分散，
 * macOS 的順序會讓漏的那幾檔排在一起，後面開 shell 的檔就撞上限（2026-10-15 每日 macOS CI，8 條紅）。
 */
export function closeLeakedShells(): number {
  const internals = sharedMock as unknown as { shells?: Map<string, unknown>; loginPanes?: Map<string, unknown> }
  // mock 改了欄位名就讓這裡直接丟錯，不要悄悄變成什麼都沒清。
  if (!(internals.shells instanceof Map) || !(internals.loginPanes instanceof Map)) throw new Error('MockTransport 的 shells／loginPanes 欄位不見了，更新 closeLeakedShells')
  const n = internals.shells.size
  internals.shells.clear()
  internals.loginPanes.clear()
  return n
}

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
    setPollTick(() => manualMockClock.advance(POLL_TICK_MS))
  })
  after(() => {
    setPollTick(null)
    useManualMockClock(false)
    // 用共用 mock 的測試檔收尾都會經過這裡：漏開的 shell 不留給後面的檔案。
    closeLeakedShells()
  })
}
