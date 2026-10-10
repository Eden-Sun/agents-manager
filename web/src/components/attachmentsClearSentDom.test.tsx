import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { useAttachments } from './attachmentsHelpers'
import { AttachTray } from './Attachments'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

test('clearSent 只移除指定附件，並釋放它的檔案指紋', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  let attachments!: ReturnType<typeof useAttachments>
  function Fixture() {
    attachments = useAttachments('b1', 'b1')
    return <AttachTray items={attachments.items} onRemove={attachments.remove} onRetry={attachments.retry} />
  }
  await mount(<Fixture />)
  const first = new File([new Uint8Array([1])], 'first.pdf', { type: 'application/pdf', lastModified: 10 })
  const second = new File([new Uint8Array([2])], 'second.pdf', { type: 'application/pdf', lastModified: 20 })
  await act(async () => attachments.add([first, second]))
  await until(() => attachments.items.length === 2 && attachments.items.every((item) => item.id), '兩張附件都有 id')
  const firstId = attachments.items[0].id!
  const secondId = attachments.items[1].id!

  await act(async () => attachments.clearSent([firstId]))
  assert.equal(attachments.items.length, 1)
  assert.equal(attachments.items[0].id, secondId)

  await act(async () => attachments.add([first]))
  await until(() => attachments.items.length === 2 && attachments.items.some((item) => item.name === 'first.pdf' && item.id), '同一檔案可再次加入')
})
