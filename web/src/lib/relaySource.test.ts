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

test('群集派工：收件端是接手的成員，標頭寫「ops → hk2（haiku 群集）」', () => {
  const l = relaySource({ fromId: 'm', fromName: 'ops', toName: 'hk2', unverified: false, cluster: 'haiku', toIsAssignee: true })
  assert.equal(l.text, 'ops → hk2（haiku 群集）')
})

test('群集派工的結果通知：寄件端是成員', () => {
  const l = relaySource({ fromId: 'k', fromName: 'hk2', toName: 'ops', unverified: false, cluster: 'haiku', fromIsAssignee: true })
  assert.equal(l.text, 'hk2（haiku 群集） → ops')
})

test('一般派工不受影響', () => {
  assert.equal(relaySource({ fromId: 'm', fromName: 'ops', toName: 'hk2', unverified: false }).text, 'ops → hk2')
  assert.equal(relaySource({ fromId: 'm', fromName: 'ops', toName: 'hk2', unverified: false, cluster: null, toIsAssignee: true }).text, 'ops → hk2')
})
