import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { AttachButton } from './AttachButton'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

const pump = async (rounds = 5) => {
  for (let i = 0; i < rounds; i++) await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
}

async function withCopyTimer(body: () => Promise<void>): Promise<void> {
  const timers = installManualTimers()
  try {
    await body()
  } finally {
    await act(async () => timers.clock.advance(2000))
    timers.restore()
  }
}

test('複製失敗：彈出的說明不寫「已複製到剪貼簿」，按鈕也不顯示已複製', async () => {
  const doc = document as unknown as { execCommand?: unknown }
  const savedExec = doc.execCommand
  const savedClipboard = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
  Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })
  doc.execCommand = () => false
  try {
    await withCopyTimer(async () => {
      await mount(<AttachButton command="herdr --remote mac_builder --session agents-manager" />)
      await click(document.querySelector<HTMLButtonElement>('.attach-btn')!)
      await pump()
      const pop = document.querySelector<HTMLElement>('.attach-pop')
      assert.ok(pop, '點了之後要彈出說明')
      const hint = pop.querySelector('.attach-hint')?.textContent ?? ''
      assert.ok(!hint.includes('已複製到剪貼簿'), `複製失敗卻說已複製：${hint}`)
      assert.ok(!document.querySelector('.attach-btn')?.classList.contains('copied'), '失敗不該亮「已複製」')
    })
  } finally {
    doc.execCommand = savedExec
    if (savedClipboard) Object.defineProperty(navigator, 'clipboard', savedClipboard)
    else delete (navigator as unknown as { clipboard?: unknown }).clipboard
  }
})

test('複製成功：說明照寫「已複製到剪貼簿」', async () => {
  const doc = document as unknown as { execCommand?: unknown }
  const savedExec = doc.execCommand
  doc.execCommand = () => true
  try {
    await withCopyTimer(async () => {
      await mount(<AttachButton command="herdr --remote mac_builder --session agents-manager" />)
      await click(document.querySelector<HTMLButtonElement>('.attach-btn')!)
      await pump()
      const hint = document.querySelector('.attach-pop .attach-hint')?.textContent ?? ''
      assert.ok(hint.includes('已複製到剪貼簿'), hint)
    })
  } finally {
    doc.execCommand = savedExec
  }
})
