import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { SentViaTag } from './SentViaTag.tsx'
import { toMessage } from '../api/normalize.ts'

test('插隊、補充的使用者訊息各有小標；一般送出與回覆不畫', () => {
  assert.match(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: 'supplement' }} />), />補充</)
  assert.match(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: 'send_now' }} />), />插隊</)
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: null }} />), '')
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'assistant', sent_via: 'supplement' }} />), '')
})

test('daemon 的 sent_via 原樣帶進來，認不得的值當一般送出', () => {
  const base = { id: 'm1', conversation_id: 'c', role: 'user', content: 'x', source: 'web', created_at: '2026-09-28T00:00:00Z' }
  assert.equal(toMessage({ ...base, sent_via: 'supplement' })?.sent_via, 'supplement')
  assert.equal(toMessage({ ...base, sent_via: 'send_now' })?.sent_via, 'send_now')
  assert.equal(toMessage({ ...base, sent_via: 'weird' })?.sent_via, null)
  assert.equal(toMessage(base)?.sent_via, null)
})
