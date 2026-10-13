/**
 * 側欄專案列的路徑縮寫（`shortPath`）：超過 36 字時取尾端 35 個字，截點不能落在 emoji 代理對中間（同 #1692）。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('專案路徑超過 36 字：縮寫取尾端，不切壞 emoji 代理對（路徑文字不能有落單的代理字元）', async () => {
  // 第 32、33 個 UTF-16 單位是 😀 的代理對；尾端 35 個 UTF-16 單位剛好從低位代理字元開始。
  const path = `/${'x'.repeat(31)}😀${'b'.repeat(34)}`
  useStore.setState({
    projects: [{ id: 'p0', label: 'proj', path, host: 'local' }],
    bots: [],
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    connected: true,
    defaultConnected: true,
    socket: 'open',
  } as never)
  await mount(<Sidebar />)
  const text = document.querySelector<HTMLElement>('.project-path')?.textContent ?? ''
  assert.equal(text, `…😀${'b'.repeat(34)}`)
  assert.doesNotMatch(text, /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/, '不能留下落單的代理字元')
})
