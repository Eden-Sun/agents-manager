/** `useStableCallback`：身分不變，但呼叫時永遠跑最新一次 render 的閉包（不能拿到過期的 state）。 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStableCallback } from './useStableCallback'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('函式身分不變，呼叫的是最新一次 render 的閉包', async () => {
  const seen: Array<(x: number) => number> = []
  function Probe({ n }: { n: number }) {
    const fn = useStableCallback((x: number) => x + n)
    seen.push(fn)
    return null
  }
  // 同一個根連續 render 三次（n 不同）：用 `mount` 掛第一次，之後 `act` 重畫同一棵樹。
  const { createRoot } = await import('react-dom/client')
  const host = document.createElement('div')
  const root = createRoot(host)
  await act(async () => root.render(<Probe n={1} />))
  await act(async () => root.render(<Probe n={10} />))
  await act(async () => root.render(<Probe n={100} />))
  assert.ok(seen.length >= 3)
  assert.ok(seen.every((f) => f === seen[0]), '每次 render 都是同一個函式')
  assert.equal(seen[0](1), 101, '呼叫時用的是最後一次 render 的 n')
  await act(async () => root.unmount())
})
