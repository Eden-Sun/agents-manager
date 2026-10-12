/**
 * 超過分享下載上限（64 MiB，am-base `outbox::MAX_BYTES`，回 413 `too_large`）的圖：重試不會好，不能寫「還在修，請稍等」。
 * 不發預覽請求、縮圖不掛 `<img>`、按鈕位置寫「這張圖太大了，沒辦法分享」。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { mockShareClient } from './shareMock'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'
const BIG = 65 * 1024 * 1024

test('超過 64 MiB 的圖：縮圖不掛 <img>、不發預覽請求；按鈕與放大檢視寫「太大了」，不寫「還在修」', async () => {
  const base = mockShareClient(TOKEN)
  const previews: string[] = []
  const client: ShareClient = {
    ...base,
    files: async () => [{ name: 'huge.png', size: BIG, modified_at: '2026-10-04T00:00:00Z' }],
    previewUrl: (name, version) => {
      previews.push(name)
      return base.previewUrl(name, version)
    },
    fileBlob: async () => {
      throw new Error('should not fetch')
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const li = document.querySelector('.sh-file-list li.img')!
  assert.equal(li.querySelector('img'), null, '不掛 <img>')
  assert.equal(previews.length, 0, '不發預覽請求')
  assert.equal(li.querySelector('.sh-png-note')?.textContent, '這張圖太大了，沒辦法分享')
  assert.doesNotMatch(li.textContent ?? '', /還在修/)
  await click(li.querySelector('.sh-file-open')!)
  await settle(50)
  assert.equal(document.querySelector('.sh-viewer-img'), null, '放大檢視不掛 <img>')
  assert.match(document.querySelector('.sh-viewer')!.textContent!, /太大了/)
})
