import test from 'node:test'
import assert from 'node:assert/strict'
import { relaySource } from './relaySource.ts'

test('驗證過的代發：名字 → 收件者', () => {
  const r = relaySource({ fromId: 'b1', fromName: 'alfa', toName: 'target', unverified: false })
  assert.equal(r.text, 'alfa → target')
  assert.equal(r.daemon, false)
  assert.doesNotMatch(r.title, /未驗證/)
})

test('沒帶 token 自稱的來源要標未驗證（#339）', () => {
  const r = relaySource({ fromId: 'b1', fromName: 'alfa', toName: 'target', unverified: true })
  assert.equal(r.text, 'alfa → target（未驗證）')
  assert.match(r.title, /未驗證/)
})

test('對不到 bot 退回 id，未驗證照標', () => {
  assert.equal(relaySource({ fromId: 'b9', fromName: '', toName: '', unverified: true }).text, 'b9（未驗證）')
})

test('daemon 哨符', () => {
  const r = relaySource({ fromId: 'daemon', fromName: '', toName: 'x', unverified: false })
  assert.equal(r.text, 'daemon 自動觸發')
  assert.equal(r.daemon, true)
})
