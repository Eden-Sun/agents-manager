import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { mockShareClient } from './shareMock'
import { ShareThumb, ShareImageViewer, IMG_RETRIES } from './ShareImages'
import { useState } from 'react'
import type { ShareFile } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const client = {
  ...mockShareClient('demo_share_token_0123456789'),
  previewUrl: () => '/s/t/api/files/a.png?inline=1',
}

const file = { name: 'a.png', size: 1, modified_at: null, version: 'v1' }

const fail = () =>
  act(async () => {
    document.querySelector('.sh-thumb img')!.dispatchEvent(new Event('error'))
  })

test('縮圖載入失敗先自己重試，沒有馬上變成「還在修」', async () => {
  await mount(<ShareThumb file={file} client={client} onOpen={() => {}} retryMs={20} />)
  const first = document.querySelector('img')
  assert.ok(first)
  await fail()
  assert.equal(document.querySelector('.sh-thumb img'), null)
  assert.ok(document.querySelector('.sh-thumb-broken'))
  assert.equal(document.querySelector('.sh-thumb-broken')!.getAttribute('title'), null)
  await settle(80)
  const second = document.querySelector('img')
  assert.ok(second)
  assert.notEqual(second, first)
})

test('連續失敗超過 IMG_RETRIES 次才顯示「還在修」', async () => {
  await mount(<ShareThumb file={file} client={client} onOpen={() => {}} retryMs={20} />)
  for (let i = 0; i < IMG_RETRIES + 1; i++) {
    await fail()
    await settle(120)
  }
  assert.equal(document.querySelector('.sh-thumb img'), null)
  assert.equal(document.querySelector('.sh-thumb-broken')!.getAttribute('title'), '這張圖還在修，請稍等')
  await settle(200)
  assert.equal(document.querySelector('.sh-thumb img'), null)
})

test('檔案換版從頭算', async () => {
  let updateFile!: (f: ShareFile) => void
  function Wrapper() {
    const [curFile, setCurFile] = useState<ShareFile>(file)
    updateFile = setCurFile
    return <ShareThumb file={curFile} client={client} onOpen={() => {}} retryMs={20} />
  }
  await mount(<Wrapper />)
  for (let i = 0; i < IMG_RETRIES + 1; i++) {
    await fail()
    await settle(120)
  }
  assert.equal(document.querySelector('.sh-thumb img'), null)
  assert.equal(document.querySelector('.sh-thumb-broken')!.getAttribute('title'), '這張圖還在修，請稍等')

  await act(async () => {
    updateFile({ ...file, version: 'v2' })
  })
  assert.ok(document.querySelector('.sh-thumb img'))
})

test('放大檢視的大圖失敗也重試、用完顯示「還在修」', async () => {
  await mount(<ShareImageViewer file={file} client={{ ...client, fileBlob: () => new Promise<Blob>(() => {}) }} onClose={() => {}} />)
  assert.ok(document.querySelector('img.sh-viewer-img'))
  await act(async () => {
    document.querySelector('.sh-viewer-img')!.dispatchEvent(new Event('error'))
  })
  assert.equal(document.querySelector('img.sh-viewer-img'), null)
  assert.ok(!document.body.textContent?.includes('還在修'))
})
