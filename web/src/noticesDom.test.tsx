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

test('部署通知使用頂部置中樣式，普通通知不受影響', async () => {
  await mount(<Notices />)
  await act(async () => {
    useStore.getState().notify('error', '部署等待', undefined, true)
    useStore.getState().notify('error', '其他錯誤')
  })
  assert.equal(document.querySelectorAll('.notice-top-center').length, 1)
  assert.equal(document.querySelector('.notice-top-center')?.textContent?.includes('部署等待'), true)
  assert.equal([...document.querySelectorAll('.notice')].find((n) => n.textContent?.includes('其他錯誤'))?.classList.contains('notice-top-center'), false)
})

test('#1920 重設測試 store 會清除通知的自動消失 timer', () => {
  useStore.getState().notify('error', '測試通知')
  assert.ok(pendingDomTimers().some((timer) => timer.timeout === 8000), '錯誤通知使用 8 秒 timer')
  resetStoreForTest()
  assert.equal(pendingDomTimers().length, 0, '重設 store 不應把通知 timer 留給下一個測試檔')
})

test('一般 info 通知點擊本體可關閉；error 與帶 action 通知點擊本體不關閉', async () => {
  let actionRan = false
  await mount(<Notices />)
  await act(async () => {
    useStore.getState().notify('info', '輕量訊息')
    useStore.getState().notify('error', '錯誤訊息')
    useStore.getState().notify('info', '動作訊息', { label: '重試', run: () => { actionRan = true } })
  })

  assert.equal(document.querySelectorAll('.notice').length, 3)

  const infoEl = [...document.querySelectorAll<HTMLElement>('.notice')].find((el) => el.textContent?.includes('輕量訊息'))!
  assert.ok(infoEl)
  await act(async () => {
    infoEl.click()
  })
  assert.equal([...document.querySelectorAll<HTMLElement>('.notice')].some((el) => el.textContent?.includes('輕量訊息')), false)

  const errEl = [...document.querySelectorAll<HTMLElement>('.notice')].find((el) => el.textContent?.includes('錯誤訊息'))!
  assert.ok(errEl)
  await act(async () => {
    errEl.click()
  })
  assert.equal([...document.querySelectorAll<HTMLElement>('.notice')].some((el) => el.textContent?.includes('錯誤訊息')), true)

  const actionNoticeEl = [...document.querySelectorAll<HTMLElement>('.notice')].find((el) => el.textContent?.includes('動作訊息'))!
  assert.ok(actionNoticeEl)
  await act(async () => {
    actionNoticeEl.click()
  })
  assert.equal(actionRan, false)
  assert.equal([...document.querySelectorAll<HTMLElement>('.notice')].some((el) => el.textContent?.includes('動作訊息')), true)

  const closeBtn = errEl.querySelector<HTMLButtonElement>('button[aria-label="關閉"]')!
  assert.ok(closeBtn)
  await act(async () => {
    closeBtn.click()
  })
  assert.equal([...document.querySelectorAll<HTMLElement>('.notice')].some((el) => el.textContent?.includes('錯誤訊息')), false)
})

test('自動消失前先加上 leaving 淡出，再從 DOM 移除', async () => {
  const timers = installManualTimers()
  try {
    await mount(<Notices />)
    await act(async () => {
      useStore.getState().notify('info', '淡出訊息')
    })
    await act(async () => {
      await timers.clock.advance(4000)
    })
    assert.equal(document.querySelector('.notice')?.classList.contains('leaving'), true)
    await act(async () => {
      await timers.clock.advance(200)
    })
    assert.equal(document.querySelectorAll('.notice').length, 0)
  } finally {
    timers.restore()
    resetStoreForTest()
  }
})

test('滑鼠停在或焦點在通知上時暫停自動消失，離開後才繼續倒數', async () => {
  const timers = installManualTimers()
  try {
    await mount(<Notices />)
    await act(async () => {
      useStore.getState().notify('info', '可暫停', { label: '查看', run: () => {} })
    })
    const box = document.querySelector<HTMLElement>('.notices')!
    await act(async () => {
      box.querySelector<HTMLButtonElement>('.notice-action')!.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
    })
    await act(async () => {
      await timers.clock.advance(60000)
    })
    assert.equal(document.querySelectorAll('.notice').length, 1, '暫停中不應消失')
    assert.equal(document.querySelector('.notice')?.classList.contains('leaving'), false)
    await act(async () => {
      useStore.getState().releaseNotices()
    })
    await act(async () => {
      await timers.clock.advance(16000)
    })
    assert.equal(document.querySelectorAll('.notice').length, 0, '恢復後照剩餘時間消失')
  } finally {
    timers.restore()
    resetStoreForTest()
  }
})

test('淡出中被暫停會取消淡出並保留通知', async () => {
  const timers = installManualTimers()
  try {
    await mount(<Notices />)
    await act(async () => {
      useStore.getState().notify('info', '救回來')
    })
    await act(async () => {
      await timers.clock.advance(4000)
    })
    await act(async () => {
      useStore.getState().holdNotices()
      await timers.clock.advance(1000)
    })
    const el = document.querySelector('.notice')
    assert.ok(el)
    assert.equal(el.classList.contains('leaving'), false)
    useStore.getState().releaseNotices()
  } finally {
    timers.restore()
    resetStoreForTest()
  }
})
