/**
 * 通知（toast）的無障礙（issue #1135）：live region 要一直在 DOM 裡，之後才加的通知才會被螢幕閱讀器唸；錯誤用 alert。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, pendingDomTimers, setupDom, teardownDom, unmountAll } from './testing/domHarness'
import { installManualTimers } from './testing/manualTimers'
import { resetStoreForTest, useStore } from './store/store'
import { Notices } from './App'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  useStore.setState({ notices: [] })
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

test('沒有通知時 live region 也在 DOM 裡', async () => {
  useStore.setState({ notices: [] })
  await mount(<Notices />)
  const box = document.querySelector<HTMLElement>('.notices[role="status"][aria-live="polite"]')
  assert.ok(box, 'live region 要先掛著')
  assert.equal(box.children.length, 0)
})

test('之後才加的通知出現在同一個容器裡；錯誤是 alert、一般的不是', async () => {
  const timers = installManualTimers()
  useStore.setState({ notices: [] })
  try {
    await mount(<Notices />)
    const box = document.querySelector<HTMLElement>('.notices')!
    await act(async () => {
      useStore.getState().notify('info', '好了')
      useStore.getState().notify('error', '壞了')
    })
    assert.equal(document.querySelector('.notices'), box, '同一個節點，沒有重掛')
    assert.equal(box.querySelector('.notice.error')!.getAttribute('role'), 'alert')
    assert.equal(box.querySelector('.notice.info')!.hasAttribute('role'), false)
  } finally {
    timers.restore()
  }
})

test('#1920 重設測試 store 會清除通知的自動消失 timer', () => {
  useStore.getState().notify('error', '測試通知')
  assert.ok(pendingDomTimers().some((timer) => timer.timeout === 8000), '錯誤通知使用 8 秒 timer')
  resetStoreForTest()
  assert.equal(pendingDomTimers().length, 0, '重設 store 不應把通知 timer 留給下一個測試檔')
})
