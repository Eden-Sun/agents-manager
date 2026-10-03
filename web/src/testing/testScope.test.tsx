/**
 * #770：一條 DOM 測試在負載下逾時，bun 不等它、但它的 body 還在跑。它的 `act()` 跟下一條測試的 `act()` 重疊時，
 * React 的 act 深度錯位，之後整個行程的 render 都不 flush（一條逾時拖紅 10 條）。這裡不真的讓測試逾時（那會讓整套紅），
 * 而是照 shim 的做法手動開一個 scope、留一個沒收的 act、把它標成結束，看 harness 有沒有擋住。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll } from './domHarness'
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
  ) // 馬上接住：沒接住的 rejection bun 會當成這條測試的錯
  endCurrentTest() // shim 的 afterEach／unmountAll 在逾時那一刻做的事

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
