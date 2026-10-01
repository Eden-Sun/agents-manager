/** `useStableCallback`：身分不變，但呼叫時永遠跑最新一次 render 的閉包（不能拿到過期的 state）。 */
import test, { after } from 'node:test'
import assert from 'node:assert/strict'
import { GlobalRegistrator } from '@happy-dom/global-registrator'

GlobalRegistrator.register({ url: 'http://localhost:5173' })
;(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true
const React = await import('react')
const { createRoot } = await import('react-dom/client')
const { useStableCallback } = await import('./useStableCallback.ts')

test('函式身分不變，呼叫的是最新一次 render 的閉包', async () => {
  const seen: Array<(x: number) => number> = []
  function Probe({ n }: { n: number }) {
    const fn = useStableCallback((x: number) => x + n)
    seen.push(fn)
    return null
  }
  const root = createRoot(document.createElement('div'))
  await React.act(async () => root.render(React.createElement(Probe, { n: 1 })))
  await React.act(async () => root.render(React.createElement(Probe, { n: 10 })))
  await React.act(async () => root.render(React.createElement(Probe, { n: 100 })))
  assert.ok(seen.length >= 3)
  assert.ok(seen.every((f) => f === seen[0]), '每次 render 都是同一個函式')
  assert.equal(seen[0](1), 101, '呼叫時用的是最後一次 render 的 n')
  await React.act(async () => root.unmount())
})

after(() => {
  void GlobalRegistrator.unregister()
})
