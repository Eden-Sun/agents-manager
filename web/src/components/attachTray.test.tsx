import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { AttachTray } from './Attachments.tsx'
import type { Pending } from './attachmentUpload'

const MB = 1024 * 1024
const card = (over: Partial<Pending>): Pending => ({
  key: 'a1', fp: 'f', name: 'big.zip', size: 12.6 * MB, isImage: false, previewUrl: '',
  compressing: false, loaded: 0, id: null, error: null, retryable: true, ...over,
})
const render = (it: Pending) => renderToStaticMarkup(<AttachTray items={[it]} onRemove={() => {}} onRetry={() => {}} />)

test('上傳中：文字寫已傳／總大小／百分比，進度條帶 aria 數值', () => {
  const html = render(card({ loaded: 3.2 * MB }))
  assert.match(html, /3\.2 \/ 12\.6 MB · 25%/)
  assert.match(html, /role="progressbar"/)
  assert.match(html, /aria-valuenow="25"/)
  assert.match(html, /aria-valuetext="3\.2 \/ 12\.6 MB · 25%"/)
  assert.match(html, /width:25%/)
  assert.doesNotMatch(html, /上傳中…/)
})

test('壓縮中：只寫「壓縮中…」，還沒有進度條', () => {
  const html = render(card({ isImage: true, previewUrl: 'blob:x', compressing: true }))
  assert.match(html, /壓縮中…/)
  assert.doesNotMatch(html, /progressbar/)
})

test('失敗：寫出原因與重試鍵；超過上限的不給重試', () => {
  const html = render(card({ error: '連線中斷，上傳沒有完成' }))
  assert.match(html, /失敗：連線中斷，上傳沒有完成/)
  assert.match(html, /重試/)
  assert.doesNotMatch(html, /progressbar/)
  assert.doesNotMatch(render(card({ error: '超過上限', retryable: false })), /重試/)
})

test('好了：沒有狀態列也沒有進度條', () => {
  const html = render(card({ id: 'att1', loaded: 12.6 * MB }))
  assert.doesNotMatch(html, /progressbar|失敗|壓縮中/)
})
