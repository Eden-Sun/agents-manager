import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, TerminalSnapshot } from '../api/types'
import { BlockedModal } from './BlockedModal'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

const MENU = ['問題？', '', '❯ 1. 甲', '  2. 乙', '', 'Enter to select · ↑/↓ to navigate · Esc to cancel'].join('\n')

async function open() {
  let deliver!: (snap: TerminalSnapshot) => void
  useStore.setState({
    selectedBotId: 'b1', bots: [{ id: 'b1', kind: 'claude' } as Bot],
    readTerminal: () => new Promise((resolve) => { deliver = resolve }),
  })
  await mount(<BlockedModal botId="b1" onClose={() => {}} />)
  await act(async () => { deliver({ text: MENU, columns: 120, rows: 40, revision: 1 } as TerminalSnapshot) })
  return document.querySelector('.blocked-modal') as HTMLElement
}

test('選單模式帶 is-menu（手機才會成底部 sheet），沒有終端原文', async () => {
  const modal = await open()
  assert.equal(modal.classList.contains('is-menu'), true)
  assert.equal(modal.querySelector('pre.blocked-modal-term'), null)
})

test('展開終端原文後不帶 is-menu（手機維持全螢幕）', async () => {
  const modal = await open()
  await act(async () => { (modal.querySelector('.bc-more') as HTMLButtonElement).click() })
  assert.ok(modal.querySelector('pre.blocked-modal-term'))
  assert.equal(modal.classList.contains('is-menu'), false)
})
