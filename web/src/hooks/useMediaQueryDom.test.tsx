/**
 * `useMediaQuery` 每次 render 都會重新訂閱：subscribe 是行內箭頭函式，React 看到身份變了就 unsubscribe＋subscribe。
 * 一個頁面有十幾顆元件用它，每次 render 都在 matchMedia 上加一次、拿掉一次監聽。這裡數 addEventListener 的次數。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useMediaQuery } from './useMediaQuery'

const Q = '(max-width: 640px)'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('同一個查詢重新 render 不會重新掛上 matchMedia 監聽', async () => {
  let adds = 0
  let removes = 0
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({
    matches: false,
    media: q,
    addEventListener: () => void (adds += 1),
    removeEventListener: () => void (removes += 1),
  })) as unknown as typeof window.matchMedia

  function Probe() {
    const [n, setN] = useState(0)
    const narrow = useMediaQuery(Q)
    return <button onClick={() => setN(n + 1)}>{`${narrow ? 'narrow' : 'wide'} ${n}`}</button>
  }

  try {
    const host = await mount(<Probe />)
    const button = host.querySelector('button')!
    for (let i = 0; i < 5; i += 1) await click(button)
    assert.equal(button.textContent, 'wide 5', '確認真的 re-render 了五次')
    assert.equal(adds, 1, `監聽只該掛一次，實際 ${adds} 次`)
    assert.equal(removes, 0, `沒有卸載就不該拿掉，實際 ${removes} 次`)
  } finally {
    window.matchMedia = original
  }
})
