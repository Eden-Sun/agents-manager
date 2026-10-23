/**
 * 「在終端開啟」：複製失敗時，彈出的說明不能還寫「已複製到剪貼簿」——使用者會照著去貼一個不在剪貼簿裡的指令。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { AttachButton } from './AttachButton'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

/** 讓 promise／React 的後續工作跑完（手動時鐘下 `settle()` 的 setTimeout 不會到期，只讓出 macrotask）。 */
const pump = async (rounds = 5) => {
  for (let i = 0; i < rounds; i++) await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
}

/**
 * AttachButton 點下去會排一個 1.6 秒後的 `setCopied(null)`。這個檔案比 1.6 秒短，timer 會越過 `teardownDom()` 才到期；
 * 剛好落在兩個測試檔之間（happy-dom 已拆、下一檔還沒掛）時 React 讀不到 `window`，整個 shard 紅（`window is not defined`，
 * 高負載才會碰到）。所以點擊期間的 timer 走手動時鐘，離開前推過 1.6 秒，讓它在元件還掛著時跑完。
 */
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
  // 兩條路都失敗：execCommand 回 false、沒有 clipboard API。
  const doc = document as unknown as { execCommand?: unknown }
  const savedExec = doc.execCommand
  const savedClipboard = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
  Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })
  doc.execCommand = () => false
  try {
    await withCopyTimer(async () => {
      await mount(<AttachButton command="herdr --remote m4p --session agents-manager" />)
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
      await mount(<AttachButton command="herdr --remote m4p --session agents-manager" />)
      await click(document.querySelector<HTMLButtonElement>('.attach-btn')!)
      await pump()
      const hint = document.querySelector('.attach-pop .attach-hint')?.textContent ?? ''
      assert.ok(hint.includes('已複製到剪貼簿'), hint)
    })
  } finally {
    doc.execCommand = savedExec
  }
})
