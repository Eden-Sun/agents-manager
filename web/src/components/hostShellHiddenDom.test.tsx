import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, fakeWebSocket } from '../testing/domHarness'
import { virtualTime } from '../testing/virtualTime'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
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
  fakeWebSocket()
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

test('分頁在背景時不輪詢快照，回前景立刻補讀', async () => {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  opened.push(shell.pane_id)
  const vt = virtualTime()
  try {
    await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
    const snapReads = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/shells/')).length
    await vt.waitFor(() => snapReads() >= 1, '掛上後第一趟快照')
    const setVisibility = (v: string) => Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
    try {
      setVisibility('hidden')
      const n0 = snapReads()
      await vt.tick(2500)
      assert.ok(snapReads() <= n0 + 1, `背景不該再讀快照：${n0} → ${snapReads()}`)

      setVisibility('visible')
      document.dispatchEvent(new Event('visibilitychange'))
      await vt.waitFor(() => snapReads() > n0, '回前景要立刻補讀一次', 200)
    } finally {
      delete (document as unknown as { visibilityState?: string }).visibilityState
    }
  } finally {
    vt.restore()
  }
})
