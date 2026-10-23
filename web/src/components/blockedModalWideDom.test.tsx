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

async function open(columns: number) {
  let deliver!: (snap: TerminalSnapshot) => void
  useStore.setState({
    selectedBotId: 'b1', bots: [{ id: 'b1', kind: 'claude' } as Bot],
    readTerminal: () => new Promise((resolve) => { deliver = resolve }),
  })
  await mount(<BlockedModal botId="b1" onClose={() => {}} />)
  await act(async () => { deliver({ text: MENU, columns, rows: 40, revision: 1 } as TerminalSnapshot) })
  return document.querySelector('.blocked-modal') as HTMLElement
}

test('選單模式（終端收起）不放寬，只標 is-menu，桌機縮成選單大小 (#1933)', async () => {
  const modal = await open(213)
  assert.ok(modal.querySelector('.bc-item'), '認得出選單')
  assert.equal(modal.classList.contains('is-wide'), false)
  assert.equal(modal.classList.contains('is-menu'), true)
})

test('展開終端原文而且欄數 > 140 才放寬；欄數不超過 140 不放寬 (#1933)', async () => {
  const modal = await open(213)
  await act(async () => { (modal.querySelector('.bc-more') as HTMLButtonElement).click() })
  assert.ok(modal.querySelector('pre.blocked-modal-term'), '終端原文已展開')
  assert.equal(modal.classList.contains('is-wide'), true)
  assert.equal(modal.classList.contains('is-menu'), false)
})

test('展開終端原文、欄數 120：不放寬', async () => {
  const modal = await open(120)
  await act(async () => { (modal.querySelector('.bc-more') as HTMLButtonElement).click() })
  assert.ok(modal.querySelector('pre.blocked-modal-term'))
  assert.equal(modal.classList.contains('is-wide'), false)
})
