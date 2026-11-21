import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
import { act, mount, pendingDomTimers, setupDom, teardownDom, unmountAll } from './domHarness'
import { endCurrentTest, runTestBody } from './testScope'
import { installManualTimers } from './manualTimers'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('已結束測試的 body：半路的 act 先收掉才開下一個；之後再進 act 被擋下；React 照常 render', async () => {
  const events: string[] = []
  let release!: () => void
  const gate = new Promise<void>((resolve) => (release = resolve))
  let leftover!: Promise<unknown>
  runTestBody('逾時的那一條', () => {
    leftover = (async () => {
      await act(async () => {
        await gate
        events.push('舊的 act 收掉')
      })
      events.push('舊的 body 想再進 act')
      await act(async () => {
        events.push('不該跑到這裡')
      })
    })()
  })
  const outcome = leftover.then(
    () => null,
    (e: unknown) => e,
  )
  endCurrentTest()

  setTimeout(release, 30)
  await act(async () => {
    events.push('下一條的 act')
  })
  assert.match(String(await outcome), /逾時的那一條.*已經結束/)
  assert.ok(events.indexOf('舊的 act 收掉') < events.indexOf('下一條的 act'), `下一條的 act 要等舊的收掉才開始：${events.join(' → ')}`)
  assert.ok(events.includes('舊的 body 想再進 act') && !events.includes('不該跑到這裡'), events.join(' → '))

  function Counter() {
    const [n, setN] = useState(0)
    return (
      <button type="button" onClick={() => setN(n + 1)}>
        {n}
      </button>
    )
  }
  const host = await mount(<Counter />)
  const button = host.querySelector('button')!
  await act(async () => button.click())
  assert.equal(button.textContent, '1', 'act 深度沒有錯位：之後的 render 照樣 flush')
})

test('測試 body 中途自己 unmountAll（例如模擬換一個分頁）不算結束：之後照樣能進 act', async () => {
  await mount(<p>舊分頁</p>)
  await unmountAll()
  const host = await mount(<p>新分頁</p>)
  assert.equal(host.textContent, '新分頁')
})

test('孤兒 act 已收束時取消 5 秒保險計時器', async () => {
  const timers = installManualTimers()
  let release!: () => void
  let leftover!: Promise<unknown>
  try {
    const gate = new Promise<void>((resolve) => (release = resolve))
    runTestBody('留下孤兒 act', () => {
      leftover = act(async () => {
        await gate
      })
    })
    const outcome = leftover.catch((e: unknown) => e)
    endCurrentTest()

    const next = act(() => {})
    assert.equal(timers.clock.pending, 1, '等待孤兒 act 時只有一個 5 秒上限 timer')
    release()
    await next
    await outcome
    assert.equal(timers.clock.pending, 0, '孤兒已完成就不應留 timer 在後續測試期間')
  } finally {
    release?.()
    timers.restore()
  }
})

test('#1920 teardownDom 等連鎖 timer 與 Promise 收束後才拆 window', async () => {
  let triggerSetState!: (v: number) => void
  function LateComponent() {
    const [val, setVal] = useState(0)
    useEffect(() => {
      triggerSetState = setVal
    }, [setVal])
    return <div>value: {val}</div>
  }

  const host = await mount(<LateComponent />)
  assert.ok(host.textContent?.includes('value: 0'))

  let lateFired = false
  const realNow = Date.now
  Date.now = () => realNow() + 10_000
  try {
    window.setTimeout(() => {
      void Promise.resolve().then(() => new Promise<void>((resolve) => {
        window.setTimeout(() => {
          lateFired = true
          triggerSetState(123)
          resolve()
        }, 70)
      }))
    }, 40)
  } finally {
    Date.now = realNow
  }

  const pending = pendingDomTimers()
  assert.ok(pending.some((p) => p.timeout === 40 && p.testName === '#1920 teardownDom 等連鎖 timer 與 Promise 收束後才拆 window'))
  assert.ok(pending.some((p) => p.stack.includes('testScope.test.tsx')), 'timer 診斷要保留排程呼叫位置')

  await teardownDom()
  assert.equal(lateFired, true, '連鎖 timer 應在 teardownDom 期間完成')

  await setupDom()
})

test('#1920 未清除的 interval 會在 teardown 報出來源測試與排程位置', async () => {
  window.setInterval(() => {}, 60_000)
  await assert.rejects(teardownDom(), (error: unknown) => {
    assert.match(String(error), /未清除的 interval/)
    assert.match(String(error), /#1920 未清除的 interval 會在 teardown 報出來源測試與排程位置/)
    assert.match(String(error), /testScope\.test\.tsx/)
    return true
  })

  assert.deepEqual(pendingDomTimers(), [], 'teardown 失敗時也要清掉剩餘 timer')
  await setupDom()
})

test('#1920 window timer 服從測試安裝的手動時鐘', async () => {
  const timers = installManualTimers()
  let fired = false
  try {
    window.setTimeout(() => { fired = true }, 120)
    await timers.clock.advance(120)
    assert.equal(fired, true, 'window.setTimeout 應委派給 globalThis 上的手動時鐘')
  } finally {
    timers.restore()
  }
})

test('#1920 teardown 取消不阻塞的長 timeout，避免回呼跑進下一個測試檔', async () => {
  let fired = false
  window.setTimeout(() => { fired = true }, 60_000)

  await teardownDom()

  assert.deepEqual(pendingDomTimers(), [])
  assert.equal(fired, false)
  await setupDom()
})

test('#1920 DOM 測試檔的 setup 等上一檔 teardown 釋放 lease', async () => {
  let acquired = false
  const nextFileSetup = setupDom().then(() => { acquired = true })
  await Promise.resolve()
  assert.equal(acquired, false, '目前測試檔還持有 DOM 時，下一檔 setup 必須等待')

  await teardownDom()
  await nextFileSetup
  assert.equal(acquired, true)
  assert.ok(document.body)

  await teardownDom()
  await setupDom()
})
