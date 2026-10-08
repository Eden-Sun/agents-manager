/**
 * #918：終端分頁換 bot／改行數時，慢回來的舊回應不能蓋到新畫面（真的掛元件進 happy-dom，`readTerminal` 由測試手動 resolve）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
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
  botId: string
  lines: number
  resolve: (s: TerminalSnapshot) => void
  reject: (e: Error) => void
}

function snapOf(text: string, pane: string): TerminalSnapshot {
  return { text, pane_id: pane, columns: 120, rows: 30 } as unknown as TerminalSnapshot
}

/** 每次 `readTerminal` 都排進 `calls`，由測試決定誰先回來。 */
function fakeReadTerminal() {
  const calls: Pending[] = []
  const read = (botId: string, _source: string, lines: number) =>
    new Promise<TerminalSnapshot>((resolve, reject) => calls.push({ botId, lines, resolve, reject }))
  return { calls, read }
}

const control = { switchBot: (_id: string) => {} }
function Harness() {
  const [id, setId] = useState('A')
  useEffect(() => {
    control.switchBot = setId
  }, [setId])
  return <TerminalTab botId={id} />
}
const switchBot = (id: string) => control.switchBot(id)

const shown = () => document.querySelector('pre.term')?.textContent ?? ''
const chip = () => document.querySelector('.term-pane-chip')?.textContent ?? ''

test('換 bot：B 先回、A 後回，畫面與 pane 晶片都是 B 的；切換當下不殘留 A 的字', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<Harness />)
  await until(() => calls.some((c) => c.botId === 'A'), 'A 的請求發出')
  const a = calls.find((c) => c.botId === 'A')!

  await act(async () => switchBot('B'))
  await until(() => calls.some((c) => c.botId === 'B'), 'B 的請求發出')
  const b = calls.find((c) => c.botId === 'B')!
  assert.ok(!shown().includes('A-OUTPUT'), '切換當下沒有 A 的字')

  await act(async () => b.resolve(snapOf('B-OUTPUT', 'pane-B')))
  await until(() => shown().includes('B-OUTPUT'), 'B 的畫面出現')
  await act(async () => a.resolve(snapOf('A-OUTPUT', 'pane-A')))
  await act(async () => {})
  assert.ok(shown().includes('B-OUTPUT') && !shown().includes('A-OUTPUT'), `慢回來的 A 不能蓋掉 B：${shown()}`)
  assert.ok(chip().includes('pane-B') && !chip().includes('pane-A'), `pane 晶片是 B 的：${chip()}`)
})

test('換 bot：A 慢慢失敗，不會變成 B 的「讀取終端失敗」', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<Harness />)
  await until(() => calls.some((c) => c.botId === 'A'), 'A 的請求發出')
  const a = calls.find((c) => c.botId === 'A')!
  await act(async () => switchBot('B'))
  await until(() => calls.some((c) => c.botId === 'B'), 'B 的請求發出')
  const b = calls.find((c) => c.botId === 'B')!
  await act(async () => b.resolve(snapOf('B-OUTPUT', 'pane-B')))
  await until(() => shown().includes('B-OUTPUT'), 'B 的畫面出現')
  await act(async () => a.reject(new Error('A 的 ssh 逾時')))
  await act(async () => {})
  assert.ok(!shown().includes('讀取終端失敗'), `A 的錯誤不屬於 B：${shown()}`)
  assert.ok(shown().includes('B-OUTPUT'))
})

test('改行數：舊行數的回應晚到，不蓋新行數的畫面', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<TerminalTab botId="A" />)
  await until(() => calls.some((c) => c.lines === 200), '200 行的請求發出')
  const old = calls.find((c) => c.lines === 200)!

  const select = document.querySelector('select') as HTMLSelectElement
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!
    setter.call(select, '50')
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await until(() => calls.some((c) => c.lines === 50), '50 行的請求發出')
  const fresh = calls.find((c) => c.lines === 50)!

  await act(async () => fresh.resolve(snapOf('FIFTY-LINES', 'pane-A')))
  await until(() => shown().includes('FIFTY-LINES'), '50 行的畫面出現')
  await act(async () => old.resolve(snapOf('TWO-HUNDRED-LINES', 'pane-A')))
  await act(async () => {})
  assert.ok(shown().includes('FIFTY-LINES') && !shown().includes('TWO-HUNDRED-LINES'), `舊行數不能蓋新畫面：${shown()}`)
})

test('在途請求被取代之後，「刷新」鈕不會卡在「刷新中…」', async () => {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<Harness />)
  await until(() => calls.some((c) => c.botId === 'A'), 'A 的請求發出')
  const button = () => [...document.querySelectorAll('button')].find((b) => /刷新/.test(b.textContent ?? ''))!
  assert.match(button().textContent ?? '', /刷新中/)
  await act(async () => switchBot('B'))
  await until(() => calls.some((c) => c.botId === 'B'), 'B 的請求發出')
  await act(async () => calls.find((c) => c.botId === 'B')!.resolve(snapOf('B-OUTPUT', 'pane-B')))
  await until(() => shown().includes('B-OUTPUT'), 'B 的畫面出現')
  assert.equal(button().textContent, '刷新', 'B 回來後按鈕恢復，A 那趟不管回不回來')
})
