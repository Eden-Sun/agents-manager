/**
 * 「在終端開啟」：複製失敗時，彈出的說明不能還寫「已複製到剪貼簿」——使用者會照著去貼一個不在剪貼簿裡的指令。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { AttachButton } from './AttachButton'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('複製失敗：彈出的說明不寫「已複製到剪貼簿」，按鈕也不顯示已複製', async () => {
  // 兩條路都失敗：execCommand 回 false、沒有 clipboard API。
  const doc = document as unknown as { execCommand?: unknown }
  const savedExec = doc.execCommand
  const savedClipboard = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
  Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })
  doc.execCommand = () => false
  try {
    await mount(<AttachButton command="herdr --remote m4p --session agents-manager" />)
    await click(document.querySelector<HTMLButtonElement>('.attach-btn')!)
    await settle(20)
    const pop = document.querySelector<HTMLElement>('.attach-pop')
    assert.ok(pop, '點了之後要彈出說明')
    const hint = pop.querySelector('.attach-hint')?.textContent ?? ''
    assert.ok(!hint.includes('已複製到剪貼簿'), `複製失敗卻說已複製：${hint}`)
    assert.ok(!document.querySelector('.attach-btn')?.classList.contains('copied'), '失敗不該亮「已複製」')
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
    await mount(<AttachButton command="herdr --remote m4p --session agents-manager" />)
    await click(document.querySelector<HTMLButtonElement>('.attach-btn')!)
    await settle(20)
    const hint = document.querySelector('.attach-pop .attach-hint')?.textContent ?? ''
    assert.ok(hint.includes('已複製到剪貼簿'), hint)
  } finally {
    doc.execCommand = savedExec
  }
})
