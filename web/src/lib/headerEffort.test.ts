import { test } from 'node:test'
import assert from 'node:assert/strict'
import { headerEffortLabel } from './headerEffort'

test('標題列強度縮寫：Medium=M、Max=X，其他原樣', () => {
  assert.deepEqual(['low', 'medium', 'high', 'xhigh', 'max', 'ultra'].map(headerEffortLabel), ['L', 'M', 'H', 'XH', 'X', 'U'])
  assert.equal(headerEffortLabel('minimal'), 'Minimal')
})
