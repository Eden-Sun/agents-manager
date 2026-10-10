/**
 * RAM 清單的 pane 畫面視窗（issue #1138）：上一趟沒回來就不發下一趟，畫面是最後回來的那一張，背景分頁不抓。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, settle } from '../testing/domHarness'
import type { MemProcess } from '../api/types'
import { MemPaneModal } from './MemPaneModal'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  await teardownDom()
})

const proc = { pane_id: 'p1', socket_path: null, argv: 'claude' } as unknown as MemProcess
const frame = (text: string) => new Response(JSON.stringify({ text, pane_id: 'p1', columns: 80, rows: 24 }), { status: 200 })

/** 換掉 fetch：只接 `/mem/processes/pane`，每一趟的回應由測試自己決定什麼時候回來。 */
function holdPaneReads() {
  const orig = globalThis.fetch
  const calls = { n: 0 }
  const resolvers: Array<(r: Response) => void> = []
  globalThis.fetch = ((input: string) => {
    if (!String(input).includes('/mem/processes/pane')) return orig(input)
    calls.n += 1
    return new Promise<Response>((r) => resolvers.push(r))
  }) as typeof fetch
  return { calls, resolvers, restore: () => (globalThis.fetch = orig) }
}

test('上一趟還沒回來就不發下一趟', async () => {
  const h = holdPaneReads()
  try {
    await mount(<MemPaneModal host="local" p={proc} onClose={() => {}} />)
    await settle(4500)
    assert.equal(h.calls.n, 1, '兩個 2 秒週期過去，第一趟還沒回來：只有那一條在飛')
  } finally {
    h.restore()
  }
})

test('回來之後才排下一趟，畫面是最後回來的那一張', async () => {
  const h = holdPaneReads()
  try {
    await mount(<MemPaneModal host="local" p={proc} onClose={() => {}} />)
    await settle(20)
    await act(async () => {
      h.resolvers[0](frame('frame-1'))
      await new Promise((r) => setTimeout(r, 20))
    })
    await settle(2200)
    assert.equal(h.calls.n, 2, '第一趟回來後約 2 秒發第二趟')
    await act(async () => {
      h.resolvers[1](frame('frame-2'))
      await new Promise((r) => setTimeout(r, 20))
    })
    await settle(50)
    assert.match(document.querySelector('.mem-pane-term')!.textContent ?? '', /frame-2/)
  } finally {
    h.restore()
  }
})
