/** #1210：刷新失敗時保留最後一份成功終端快照。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { TerminalSnapshot } from '../api/types'
import { TerminalTab } from './TerminalTab'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

interface Pending {
  resolve: (snapshot: TerminalSnapshot) => void
  reject: (error: Error) => void
}

function snapOf(text: string): TerminalSnapshot {
  return { text, pane_id: 'p1', columns: 120, rows: 30 } as unknown as TerminalSnapshot
}

function fakeReadTerminal() {
  const calls: Pending[] = []
  const read = () => new Promise<TerminalSnapshot>((resolve, reject) => calls.push({ resolve, reject }))
  return { calls, read }
}

const shown = () => document.querySelector('pre.term')?.textContent ?? ''
const refreshButton = () => [...document.querySelectorAll('button')].find((b) => b.textContent === '刷新')!

test('一趟讀取失敗：畫面留著上一份快照，另外提示失敗', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<TerminalTab botId="b1" />)
  await until(() => calls.length === 1, '首次終端讀取發出')
  await act(async () => calls[0]!.resolve(snapOf('hello')))
  await until(() => shown().includes('hello'), '成功快照出現')

  await act(async () => refreshButton().click())
  await until(() => calls.length === 2, '手動刷新發出')
  await act(async () => calls[1]!.reject(new Error('boom')))

  assert.ok(shown().includes('hello') && !shown().includes('讀取終端失敗'), `pre 保留成功快照：${shown()}`)
  const warning = document.querySelector('.term-warn[role="status"]')?.textContent ?? ''
  assert.ok(warning.includes('讀取終端失敗：boom'), `警示顯示本次錯誤：${warning}`)
})

test('之後成功：提示消失、畫面換成新快照', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<TerminalTab botId="b1" />)
  await until(() => calls.length === 1, '首次終端讀取發出')
  await act(async () => calls[0]!.resolve(snapOf('hello')))
  await until(() => shown().includes('hello'), '成功快照出現')
  await act(async () => refreshButton().click())
  await until(() => calls.length === 2, '第二次終端讀取發出')
  await act(async () => calls[1]!.reject(new Error('boom')))
  await until(() => Boolean(document.querySelector('.term-warn[role="status"]')?.textContent?.includes('boom')), '失敗提示出現')

  await act(async () => refreshButton().click())
  await until(() => calls.length === 3, '第三次終端讀取發出')
  await act(async () => calls[2]!.resolve(snapOf('world')))
  await until(() => shown().includes('world'), '最新快照出現')
  assert.ok(
    ![...document.querySelectorAll('.term-warn[role="status"]')].some((warning) =>
      Boolean(warning.textContent?.includes('讀取終端失敗')),
    ),
  )
})

test('還沒有任何快照就失敗：照舊在 pre 裡寫讀取終端失敗', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<TerminalTab botId="b1" />)
  await until(() => calls.length === 1, '首次終端讀取發出')
  await act(async () => calls[0]!.reject(new Error('boom')))
  await until(() => shown().includes('讀取終端失敗：boom'), '讀取錯誤出現在 pre')
  assert.equal(document.querySelector('.term-warn[role="status"]'), null)
})
