/**
 * 分隔線不再折成兩三列（2026-10-05 使用者截圖）：終端分頁與 `linkifyTerm` 的其他使用者都把「整列只有分隔線」的列縮到 32 個，
 * 內文一個字都不動（真的掛元件進 happy-dom）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { TerminalSnapshot } from '../api/types'
import { TerminalTab } from './TerminalTab'
import { linkifyTerm } from './termLinks'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const RULE = '─'.repeat(310)
const HALF = '─'.repeat(212)
const TEXT = [
  'I have launched the cargo test list command and will inspect the existing tests.',
  RULE,
  HALF,
  '>',
  RULE,
  `│ ${'─'.repeat(100)} │`,
  `https://example.com/a?b=1 ${'─'.repeat(100)}`,
].join('\n')

test('終端分頁：整列的分隔線縮成 32 個，內文與行內的線原樣', async () => {
  const snap = { text: TEXT, columns: 213, rows: 55 } as unknown as TerminalSnapshot
  await act(() => useStore.setState({ readTerminal: async () => snap } as never))
  await mount(<TerminalTab botId="b1" />)
  await until(() => document.querySelector('pre.term')?.textContent?.includes('cargo test') === true, '終端快照出現')
  const shown = document.querySelector('pre.term')!.textContent!.split('\n')
  assert.deepEqual(shown.filter((l) => /^─+$/.test(l)).map((l) => l.length), [32, 32, 32], '三條整列分隔線都縮到 32')
  assert.equal(shown[0], 'I have launched the cargo test list command and will inspect the existing tests.')
  assert.equal(shown[3], '>')
  assert.equal(shown[5], `│ ${'─'.repeat(100)} │`, '行內的線不動')
  assert.ok(shown[6].endsWith('─'.repeat(100)), '有前綴文字的不動')
})

test('linkifyTerm（卡住面板、shell 面板也走這裡）：沒有網址時也縮；有網址時分隔線縮、網址照連', async () => {
  const el = await mount(<pre>{linkifyTerm(`${RULE}\ntext\n${RULE}`)}</pre>)
  assert.deepEqual(el.textContent!.split('\n'), ['─'.repeat(32), 'text', '─'.repeat(32)])
  const el2 = await mount(<pre>{linkifyTerm(`see https://example.com/x\n${RULE}`, 213)}</pre>)
  assert.deepEqual(el2.textContent!.split('\n'), ['see https://example.com/x', '─'.repeat(32)])
  assert.ok(el2.querySelector('.term-link'), '網址仍是可點的連結')
})
