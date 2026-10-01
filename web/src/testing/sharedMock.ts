import { MockTransport } from '../api/mock'

/**
 * 整個行程共用的一個 `MockTransport`：store 會丟掉 `daemon_seq` 比較小的快照，每個測試檔各建一個等於「daemon 重啟」，
 * store 就不更新了；草稿的 rev 也是同理。用它的測試檔收尾要 `resetStoreForTest()`（store 是模組單例）。
 */
export const sharedMock = new MockTransport()
