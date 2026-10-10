import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, click, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { mockShareClient } from './shareMock'
import { ShareImageViewer } from './ShareImages'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const client = mockShareClient('demo_share_token_0123456789')
const file = { name: 'a.png', size: 3, modified_at: '2026-10-04T00:00:00Z' }

function Host() {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button id="opener" type="button" onClick={() => setOpen(true)}>
        開
      </button>
      <textarea aria-label="背後的輸入框" />
      {open ? <ShareImageViewer file={file} client={client} onClose={() => setOpen(false)} /> : null}
    </>
  )
}

test('放大檢視：開啟後焦點在「✕」、Tab 不離開檢視、關掉後焦點回到打開它的那顆', async () => {
  await mount(<Host />)
  const opener = document.getElementById('opener')!
  opener.focus()
  await click(opener)
  await settle(50)
  const close = document.querySelector<HTMLElement>('.sh-viewer-close')!
  assert.equal(document.activeElement, close, '進場焦點交給「✕」，不留在背後')

  for (let i = 0; i < 4; i++) {
    await keydown(document.activeElement!, 'Tab')
    assert.ok(document.activeElement!.closest('.sh-viewer'), `Tab 不能落到檢視外：${document.activeElement?.tagName}`)
  }

  await act(async () => {
    close.focus()
  })
  const buttons = [...document.querySelectorAll<HTMLElement>('.sh-viewer button:not([disabled])')]
  await keydown(close, 'Tab', { shiftKey: true })
  assert.equal(document.activeElement, buttons.at(-1), 'Shift+Tab 從「✕」繞到最後一顆')

  await keydown(document.body, 'Escape')
  await settle(50)
  assert.equal(document.querySelector('.sh-viewer'), null)
  assert.equal(document.activeElement, opener, '關掉後焦點回到打開它的那顆')
})
