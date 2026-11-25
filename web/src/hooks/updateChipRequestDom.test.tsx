import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { useUpdateChipRequest } from './useUpdateChipRequest'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
after(teardownDom)

function Probe({ kinds, opened }: { kinds: string[]; opened: string[] }) {
  useUpdateChipRequest((k) => kinds.includes(k), () => void opened.push(kinds.join(',')))
  return null
}

test('header chip opens only for its own kind, and not for a request made before it mounted', async () => {
  const opened: string[] = []
  await act(() => useStore.getState().requestUpdateChip('grok'))
  await mount(<Probe kinds={['claude', 'codex']} opened={opened} />)
  assert.deepEqual(opened, [], '掛載前的舊請求不重放')
  await act(() => useStore.getState().requestUpdateChip('grok'))
  assert.deepEqual(opened, [], '不是自己的種類不開')
  await act(() => useStore.getState().requestUpdateChip('claude'))
  assert.deepEqual(opened, ['claude,codex'])
})
