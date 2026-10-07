import { test } from 'node:test'
import assert from 'node:assert/strict'
import { toToolMap } from '../api/normalize'
import { agyQuotaErrorText } from './agyQuotaError'

test('quota_error 只在 daemon 帶了才出現，登入旗標不受影響', () => {
  const none = toToolMap({ agy: { installed: true, logged_in: true } })
  assert.equal(none.agy.quota_error, undefined)
  const withErr = toToolMap({
    agy: { installed: true, logged_in: true, quota_error: { reason: 'timeout', message: 'did not finish', at: '2026-10-07T01:00:00.000Z' } },
  })
  assert.equal(withErr.agy.logged_in, true)
  assert.deepEqual(withErr.agy.quota_error, { reason: 'timeout', message: 'did not finish', at: '2026-10-07T01:00:00.000Z' })
  // 缺欄位／亂格式一律當沒有，不編原因。
  assert.equal(toToolMap({ agy: { installed: true, quota_error: 'x' } }).agy.quota_error, undefined)
  assert.equal(toToolMap({ agy: { installed: true, quota_error: { message: 'm' } } }).agy.quota_error, undefined)
})

test('各種原因都有人話，沒見過的原因退回 daemon 的訊息', () => {
  const e = (reason: string, message = 'raw') => ({ reason, message, at: '2026-10-07T01:00:00.000Z' })
  assert.match(agyQuotaErrorText(e('timeout')), /逾時/)
  assert.match(agyQuotaErrorText(e('unreadable')), /讀不懂/)
  assert.match(agyQuotaErrorText(e('exit')), /失敗/)
  assert.match(agyQuotaErrorText(e('pane')), /pane/)
  assert.equal(agyQuotaErrorText(e('something-new', 'raw detail')), 'raw detail')
})
