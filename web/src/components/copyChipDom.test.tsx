import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { CopyChip } from './CopyChip'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

async function withClipboard(secure: boolean, writeText: (text: string) => Promise<void>, body: () => Promise<void>) {
  const context = Object.getOwnPropertyDescriptor(window, 'isSecureContext')
  const clipboard = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
  Object.defineProperty(window, 'isSecureContext', { configurable: true, value: secure })
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } })
  try { await body() } finally {
    if (context) Object.defineProperty(window, 'isSecureContext', context)
    else delete (window as unknown as { isSecureContext?: boolean }).isSecureContext
    if (clipboard) Object.defineProperty(navigator, 'clipboard', clipboard)
    else delete (navigator as unknown as { clipboard?: unknown }).clipboard
  }
}

test('HTTP 直接顯示可選取的完整 pane 和裝置碼，不寫入剪貼簿', async () => {
  let calls = 0
  await withClipboard(false, async () => { calls++ }, async () => {
    await mount(<div><CopyChip label="pane" value="agents-manager-very-long-pane-id" title="pane" /><CopyChip label="代碼" value="ABCD-EFGH" title="GitHub 裝置碼" /></div>)
    const chips = [...document.querySelectorAll('.copy-chip-selectable')]
    assert.equal(chips.length, 2)
    assert.deepEqual(chips.map(c => c.querySelector('.copy-chip-value')?.textContent), ['agents-manager-very-long-pane-id', 'ABCD-EFGH'])
    assert.ok(chips.every(c => c.textContent?.includes('長按選取後複製')))
    assert.equal(document.querySelectorAll('button.copy-chip').length, 0)
    await act(async () => { chips[0].dispatchEvent(new MouseEvent('click', { bubbles: true })) })
    assert.equal(calls, 0)
  })
})

test('secure context 複製成功；拒絕時改為可選取文字', async () => {
  const timers = installManualTimers()
  const copies: string[] = []
  try {
    await withClipboard(true, async (value) => { copies.push(value); if (copies.length === 2) throw new Error('denied') }, async () => {
      await mount(<CopyChip label="pane" value="w0:p1" title="pane" />)
      await click(document.querySelector('button.copy-chip')!)
      await act(async () => {})
      assert.deepEqual(copies, ['w0:p1'])
      assert.match(document.body.textContent ?? '', /已複製/)
      await act(async () => timers.clock.advance(1200))
      await click(document.querySelector('button.copy-chip')!)
      await act(async () => {})
      assert.match(document.querySelector('.copy-chip-selectable')?.textContent ?? '', /複製失敗，長按選取/)
    })
  } finally { timers.restore() }
})
