import test from 'node:test'
import assert from 'node:assert/strict'
import { mergeMessages, ShareHttpError, shareErrorText, toShareFiles, toShareMessage, toSharePage, tokenFromLocation } from './shareModel.ts'

const T = 'AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abcd'

test('token：正式網址 /s/<token>，dev 用 ?token=；不是 base64url 的一律不認', () => {
  assert.equal(tokenFromLocation({ pathname: `/s/${T}`, search: '' }), T)
  assert.equal(tokenFromLocation({ pathname: `/s/${T}/`, search: '' }), T)
  assert.equal(tokenFromLocation({ pathname: '/share.html', search: `?token=${T}` }), T)
  assert.equal(tokenFromLocation({ pathname: '/s/short', search: '' }), null, '太短')
  assert.equal(tokenFromLocation({ pathname: '/s/..%2F..%2Fapi%2Fstate', search: '' }), null, '不能拿路徑字元當 token')
  assert.equal(tokenFromLocation({ pathname: `/s/${T}/api/messages`, search: '' }), null)
  assert.equal(tokenFromLocation({ pathname: '/', search: '' }), null)
})

test('訊息只留 user／assistant，附件只有名字；system／tool 之類不畫', () => {
  assert.deepEqual(toShareMessage({ id: 'a', role: 'user', text: 'hi', created_at: 't', attachments: [{ name: 'x.txt', path: '/secret' }] }), {
    id: 'a', role: 'user', text: 'hi', created_at: 't', attachments: [{ name: 'x.txt' }],
  })
  assert.equal(toShareMessage({ id: 'b', role: 'system', text: 'internal' }), null)
  assert.equal(toShareMessage({ role: 'assistant', text: 'no id' }), null)
  const page = toSharePage({ bot_name: 'support', status: 'working', messages: [{ id: '1', role: 'assistant', content: 'yo' }, { id: '2', role: 'tool' }], has_more: true })
  assert.equal(page.bot_name, 'support')
  assert.equal(page.status, 'working')
  assert.deepEqual(page.messages.map((m) => m.text), ['yo'])
})

test('合併：同 id 取新的、照時間排', () => {
  const m = (id: string, at: string, text = id) => ({ id, role: 'user' as const, text, created_at: at, attachments: [] })
  const out = mergeMessages([m('b', '2'), m('a', '1')], [m('b', '2', 'B2'), m('c', '3')])
  assert.deepEqual(out.map((x) => x.text), ['a', 'B2', 'c'])
})

test('檔案清單：沒有名字的丟掉', () => {
  assert.deepEqual(toShareFiles({ files: [{ name: 'a.txt', size: 3 }, { size: 1 }] }), [{ name: 'a.txt', size: 3, modified_at: null }])
})

test('錯誤給 end user 的話：404＝連結失效、429 帶秒數、不洩漏內部細節', () => {
  assert.equal(shareErrorText(new ShareHttpError(404), 'load'), '這個分享連結已失效。')
  assert.match(shareErrorText(new ShareHttpError(429, 30), 'send'), /30 秒/)
  assert.match(shareErrorText(new ShareHttpError(413), 'upload'), /25 MB/)
  assert.doesNotMatch(shareErrorText(new Error('ECONNREFUSED 127.0.0.1:7790'), 'send'), /127\.0\.0\.1/)
})
