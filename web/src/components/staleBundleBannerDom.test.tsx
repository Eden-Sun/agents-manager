import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { StaleBundleBanner } from './StaleBundleBanner'
import { resetStaleBundleForTest } from '../lib/staleBundle'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStaleBundleForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const realFetch = globalThis.fetch
function serve(script: string) {
  globalThis.fetch = (async () => new Response(`<script type="module" src="${script}"></script>`, { status: 200 })) as typeof fetch
}
after(() => {
  globalThis.fetch = realFetch
})

async function openSocket() {
  await act(() => useStore.setState({ socket: 'open' } as never))
}

test('換版：連上後跳橫幅；相同版不跳', async () => {
  serve('/assets/index-NEW.js')
  await mount(<StaleBundleBanner loaded="/assets/index-OLD.js" />)
  await openSocket()
  await until(() => document.querySelector('.stale-bundle-banner') !== null)
  assert.match(document.querySelector('.stale-bundle-banner')!.textContent ?? '', /請關閉後重新開啟/)
  assert.ok(document.querySelector('.stale-bundle-reload'))
})

test('相同版不跳橫幅', async () => {
  serve('/assets/index-SAME.js')
  await mount(<StaleBundleBanner loaded="/assets/index-SAME.js" />)
  await openSocket()
  await new Promise((r) => setTimeout(r, 20))
  assert.equal(document.querySelector('.stale-bundle-banner'), null)
})

test('dev 模式（載入的不是 /assets/）跳過，不查', async () => {
  let calls = 0
  globalThis.fetch = (async () => {
    calls += 1
    return new Response('', { status: 200 })
  }) as typeof fetch
  await mount(<StaleBundleBanner loaded={null} />)
  await openSocket()
  await new Promise((r) => setTimeout(r, 20))
  assert.equal(calls, 0)
  assert.equal(document.querySelector('.stale-bundle-banner'), null)
})

test('收起後同一版不再跳；30 秒內不重查', async () => {
  serve('/assets/index-NEW.js')
  await mount(<StaleBundleBanner loaded="/assets/index-OLD.js" />)
  await openSocket()
  await until(() => document.querySelector('.stale-bundle-banner') !== null)
  await click(document.querySelector('.stale-bundle-dismiss')!)
  assert.equal(document.querySelector('.stale-bundle-banner'), null, '收起')
  await act(() => useStore.setState({ socket: 'connecting' } as never))
  await act(() => useStore.setState({ socket: 'open' } as never))
  await new Promise((r) => setTimeout(r, 20))
  assert.equal(document.querySelector('.stale-bundle-banner'), null, '同一版不再跳')
})

async function until(pred: () => boolean, ms = 2000) {
  const start = Date.now()
  while (!pred()) {
    if (Date.now() - start > ms) throw new Error('timeout')
    await act(() => new Promise((r) => setTimeout(r, 10)))
  }
}
