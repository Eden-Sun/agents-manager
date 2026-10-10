import test from 'node:test'
import assert from 'node:assert/strict'
import { fmtSize, mergeMessages, ShareHttpError, shareErrorText, shareSafeHref, toShareFiles, toShareMessage, toSharePage, toStatus, tokenFromLocation } from './shareModel.ts'

const T = 'AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abcd'

test('一頁超過 100 則或單則超長文字會被截掉，不能整包塞進頁面', () => {
  const huge = 'x'.repeat(200_000)
  const messages = Array.from({ length: 250 }, (_, i) => ({ id: `m${i}`, role: 'assistant', text: i === 0 ? huge : 'ok', created_at: '2024-01-01T00:00:00Z' }))
  const page = toSharePage({ bot_name: 'b', status: 'idle', has_more: true, messages })
  assert.ok(page.messages.length <= 100)
  assert.ok(page.messages[0].text.length < huge.length)
  assert.match(page.messages[0].text, /內容過長/)
})

test('分享頁連結只留 http／https', () => {
  assert.equal(shareSafeHref('https://example.com/a'), 'https://example.com/a')
  assert.equal(shareSafeHref('javascript:alert(1)'), undefined)
  assert.equal(shareSafeHref('data:text/html,x'), undefined)
  assert.equal(shareSafeHref('//evil.example/x'), undefined)
  assert.equal(shareSafeHref(' javascript:alert(1)'), undefined)
})

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
  assert.deepEqual(toShareFiles({ files: [{ name: 'a.txt', size: 3 }, { size: 1 }] }), [{ name: 'a.txt', size: 3, modified_at: null, version: null }])
})

test('檔案清單：讀得到 daemon 給的 version，沒有時為 null', () => {
  const [a, b] = toShareFiles({ files: [{ name: 'a.svg', size: 3, modified_at: '2026-10-04T00:00:00Z', version: '1-3-5-7' }, { name: 'b.svg', size: 3 }] })
  assert.equal(a.version, '1-3-5-7')
  assert.equal(b.version, null)
})

test('錯誤給 end user 的話：404＝連結失效、429 帶秒數、不洩漏內部細節', () => {
  assert.equal(shareErrorText(new ShareHttpError(404), 'load'), '這個分享連結已失效。')
  assert.match(shareErrorText(new ShareHttpError(429, 30), 'send'), /30 秒/)
  assert.match(shareErrorText(new ShareHttpError(413), 'upload'), /25 MB/)
  assert.match(shareErrorText(new ShareHttpError(507), 'send'), /空間滿了/)
  assert.doesNotMatch(shareErrorText(new Error('ECONNREFUSED 127.0.0.1:7790'), 'send'), /127\.0\.0\.1/)
})

test('400 unknown_attachment：文案說要移除、重新選，不再講「再試一次」（#1097）', () => {
  const msg = shareErrorText(new ShareHttpError(400, null, 'unknown_attachment'), 'send')
  assert.match(msg, /移除/)
  assert.match(msg, /重新選/)
  assert.doesNotMatch(msg, /unknown/)
  assert.equal(shareErrorText(new ShareHttpError(400), 'send'), '送出失敗，請再試一次；你打的字還在。', '其他 400 照舊')
  assert.equal(new ShareHttpError(404).reason, null)
})

test('status 是 daemon 的 lamp：working／blocked／starting 都算「還沒回完」', () => {
  for (const s of ['working', 'blocked', 'starting']) assert.equal(toStatus({ status: s }), 'working', s)
  for (const s of ['idle', 'offline', 'unknown', '']) assert.equal(toStatus({ status: s }), 'idle', s)
  assert.match(shareErrorText(new ShareHttpError(409), 'send'), /你打的字還在/)
})

test('fmtSize 邊界：差一點滿 1 MB 寫 1.0 MB，不是 1024 KB', () => {
  assert.equal(fmtSize(1024 * 1024 - 1), '1.0 MB')
  assert.equal(fmtSize(10 * 1024 - 1), '10 KB')
  assert.equal(fmtSize(1023), '1023 B')
  assert.equal(fmtSize(1536), '1.5 KB')
  assert.equal(fmtSize(20 * 1024), '20 KB')
  assert.equal(fmtSize(25 * 1024 * 1024), '25.0 MB')
})
