/**
 * `usePollWhileVisible`：背景分頁的每一拍都不該打 daemon；回到前景補一次；卸載後不再跑。
 * 用手動時鐘數 tick，不靠牆鐘。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { usePollWhileVisible } from './usePollWhileVisible'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

function setVisibility(v: 'visible' | 'hidden') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
  document.dispatchEvent(new Event('visibilitychange'))
}

function Probe({ onTick }: { onTick: () => void }) {
  usePollWhileVisible(onTick, 30_000)
  return null
}

test('背景分頁不打：十拍都不跑；回前景補一次；之後照拍子跑', async () => {
  const { clock, restore } = installManualClockTimers()
  let ticks = 0
  try {
    setVisibility('hidden')
    await mount(<Probe onTick={() => { ticks += 1 }} />)
    clock.advance(30_000 * 10)
    assert.equal(ticks, 0, '背景時每一拍都該跳過')

    setVisibility('visible')
    assert.equal(ticks, 1, '回到前景立刻補一次')

    clock.advance(30_000)
    assert.equal(ticks, 2, '前景之後照原本的拍子')
  } finally {
    restore()
  }
})

test('卸載後計時器與 visibilitychange 監聽都拿掉', async () => {
  const { clock, restore } = installManualClockTimers()
  let ticks = 0
  try {
    setVisibility('visible')
    await mount(<Probe onTick={() => { ticks += 1 }} />)
    clock.advance(30_000)
    assert.equal(ticks, 1)
    await unmountAll()
    clock.advance(30_000 * 5)
    setVisibility('visible')
    assert.equal(ticks, 1, '卸載後不該再跑')
  } finally {
    restore()
  }
})
