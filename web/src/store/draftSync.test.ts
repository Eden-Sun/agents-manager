import test from 'node:test'
import assert from 'node:assert/strict'
import { draftsClearedElsewhere } from './draftSync'

const raw = (o: Record<string, string>) => JSON.stringify(o)

test('別的分頁送出清掉的草稿：這裡沒動過就跟著清', () => {
  assert.deepEqual(draftsClearedElsewhere({ 'bot:b1': '跑測試' }, raw({ 'bot:b1': '跑測試' }), raw({})), ['bot:b1'])
})

test('這裡改過的字不動；對方新增的也不套過來', () => {
  assert.deepEqual(draftsClearedElsewhere({ 'bot:b1': '跑測試 再加一句' }, raw({ 'bot:b1': '跑測試' }), raw({})), [])
  assert.deepEqual(draftsClearedElsewhere({}, raw({}), raw({ 'bot:b2': 'x' })), [])
})

test('壞掉的值不丟例外', () => {
  assert.deepEqual(draftsClearedElsewhere({ a: 'x' }, '{oops', null), [])
})
