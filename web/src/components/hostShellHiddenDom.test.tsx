/**
 * #1129：host shell 面板在分頁背景時不輪詢快照（同 #909 的 blocked 輪詢），回前景立刻補讀一次。
 * 桌機寬度、鍵盤直通開著＝原本每 0.25 秒一趟，正是最需要停下來的情況。真的掛 `HostShellPanel` 進 happy-dom。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
// 共用的 mock 一個主機最多 8 個 shell，而且跨測試檔累積：自己開的自己關。
const opened: string[] = []
afterEach(async () => {
  await unmountAll()
  for (const pane of opened.splice(0)) await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(pane)}?confirm=true`).catch(() => {})
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
beforeEach(() => {
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

test('分頁在背景時不輪詢快照，回前景立刻補讀', async () => {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  opened.push(shell.pane_id)
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await settle(300)
  const snapReads = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/shells/')).length
  const setVisibility = (v: string) => Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
  try {
    setVisibility('hidden')
    const n0 = snapReads()
    await settle(2500)
    // 容許正在飛的那一趟。
    assert.ok(snapReads() <= n0 + 1, `背景不該再讀快照：${n0} → ${snapReads()}`)

    setVisibility('visible')
    document.dispatchEvent(new Event('visibilitychange'))
    await settle(200)
    assert.ok(snapReads() > n0, '回前景要立刻補讀一次')
  } finally {
    delete (document as unknown as { visibilityState?: string }).visibilityState
  }
})
