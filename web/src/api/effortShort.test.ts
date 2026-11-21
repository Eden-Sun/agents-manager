import test from 'node:test'
import assert from 'node:assert/strict'
import { effortShort } from './types.ts'

test('強度縮寫：常見等級一兩個字，不認得的退回首字大寫全名', () => {
  assert.equal(effortShort('medium'), 'M')
  assert.equal(effortShort('high'), 'H')
  assert.equal(effortShort('xhigh'), 'XH')
  assert.equal(effortShort('low'), 'L')
  assert.equal(effortShort('max'), 'Max')
  assert.equal(effortShort('Medium'), 'M', '大小寫不拘')
})
