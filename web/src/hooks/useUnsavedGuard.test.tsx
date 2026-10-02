import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useUnsavedGuard } from './useUnsavedGuard'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

let setDirty: (v: boolean) => void = () => {}
function Probe() {
  const [dirty, set] = useState(false)
  setDirty = set
  useUnsavedGuard(dirty)
  return null
}

const unload = () => {
  const ev = new Event('beforeunload', { cancelable: true })
  window.dispatchEvent(ev)
  return ev.defaultPrevented
}

test('有未儲存變更時關分頁／重整會被攔，沒有就不攔，元件卸載後也不留監聽', async () => {
  await mount(<Probe />)
  assert.equal(unload(), false)
  await act(async () => setDirty(true))
  assert.equal(unload(), true)
  await act(async () => setDirty(false))
  assert.equal(unload(), false)
  await act(async () => setDirty(true))
  await unmountAll()
  assert.equal(unload(), false, '卸載之後不能還擋著關分頁')
})
