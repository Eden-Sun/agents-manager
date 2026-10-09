/**
 * `useCacheTick`：active 從 false 變 true 時要立刻換成新的 now，不能拿掛載時的舊時間等 15 秒
 * （否則快取倒數會把已涼的快取算成滿格、不警告）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useCacheTick } from './useCacheTick'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

// 測試要在掛載後改 active：用一個能從外面叫的 setter 代替 props 重畫（domHarness 沒有 rerender）。
let setActive: (v: boolean) => void = () => {}
function Probe() {
  const [active, set] = useState(false)
  setActive = set
  return <span>{useCacheTick(active)}</span>
}

test('active 從 false 變 true：立刻換成新的 now，不等 15 秒', async () => {
  const realNow = Date.now
  try {
    await mount(<Probe />)
    const shown = () => document.querySelector('span')!.textContent
    const mountedAt = shown()
    assert.ok(mountedAt, '掛載時先畫出一個數字')

    const fake = realNow() + 3_600_000
    Date.now = () => fake
    await act(async () => setActive(true))
    await settle(5)
    assert.equal(shown(), String(fake), 'active 變 true 後要立刻補一次，不是等 15 秒的 interval')
  } finally {
    Date.now = realNow
  }
})
