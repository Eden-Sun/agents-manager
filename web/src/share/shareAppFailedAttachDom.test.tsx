/**
 * 附件沒傳上去（413、415、重試也失敗）時，按送出不能靜悄悄只送文字：bot 會以為那張照片沒有被送來。
 * 主 UI 的同類情況會擋下來並說明（`ATTACHMENT_FAILED_NOTICE`）；分享頁也要擋，讓人去移除或再試一次。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll, typeInto } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { mockShareClient } from './shareMock'
import { ShareHttpError } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

test('附件傳不上去時按送出：不送出文字，提示先處理附件', async () => {
  let sent = 0
  const base = mockShareClient(TOKEN)
  const client: ShareClient = {
    ...base,
    upload: async () => {
      throw new ShareHttpError(413)
    },
    send: async (...args) => {
      sent++
      return base.send(...args)
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: [new File(['x'], 'photo.jpg', { type: 'image/jpeg' })] })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await settle(50)
  await settle(50)
  assert.match(document.querySelector('.sh-pending')!.textContent!, /這個檔案太大了/)
  await typeInto(document.querySelector('textarea[aria-label="訊息"]')!, '這是我的照片')
  await settle(20)
  const send = document.querySelector<HTMLButtonElement>('.sh-send')!
  if (!send.disabled) await click(send)
  await settle(50)
  assert.equal(sent, 0, '附件沒傳上去，文字不能單獨送出去')
  assert.ok(document.querySelector('.sh-send-err')?.textContent?.includes('附件'), '要說明為什麼送不出去')
})
