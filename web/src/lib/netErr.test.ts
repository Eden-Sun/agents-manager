import test from 'node:test'
import assert from 'node:assert/strict'
import { gatewayErrText, networkErrText } from './netErr'

test('瀏覽器的英文網路錯誤講成人話，其他錯誤不動', () => {
  assert.match(networkErrText(new TypeError('Failed to fetch'))!, /連不上 daemon/)
  assert.match(networkErrText(new TypeError('Load failed'))!, /連不上 daemon/)
  assert.match(networkErrText(Object.assign(new Error('x'), { name: 'AbortError' }))!, /逾時/)
  assert.equal(networkErrText(new TypeError('Cannot read properties of undefined')), null)
  assert.equal(networkErrText(new Error('boom')), null)
})

test('沒帶 daemon 內容的 502/503/504 才改寫', () => {
  assert.match(gatewayErrText(502, false)!, /暫時不可用/)
  assert.equal(gatewayErrText(502, true), null)
  assert.equal(gatewayErrText(500, false), null)
})
