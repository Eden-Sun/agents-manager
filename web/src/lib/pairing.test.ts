import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import {
  formatPairCode,
  isPairingRequired,
  normalizePairCode,
  pairCodeErrorText,
  pairErrorText,
  pairRetryAfterSecs,
  waitText,
} from './pairing.ts'

test('輸入配對碼時大小寫、空白與連字號都不算', () => {
  assert.equal(normalizePairCode('abc-def'), 'ABCDEF')
  assert.equal(normalizePairCode(' A b C - d E f '), 'ABCDEF')
  assert.equal(normalizePairCode('ABC DEF'), 'ABCDEF')
  // 全形空白與使用者常打的破折號一起吃掉。
  assert.equal(normalizePairCode('ABC—DEF　'), 'ABCDEF')
})

test('六碼顯示成 ABC-DEF，長度不對就原樣吐回去', () => {
  assert.equal(formatPairCode('abcdef'), 'ABC-DEF')
  assert.equal(formatPairCode('ABC-DEF'), 'ABC-DEF')
  assert.equal(formatPairCode('abcd'), 'ABCD')
})

test('只有 403 pairing_required 算「這台裝置還沒配對」', () => {
  assert.equal(isPairingRequired(new ApiError(403, { error: 'pairing_required' }, 'x')), true)
  // 同樣是 403 的另一種：Host/Origin 不是本機，那是設定問題，不該給配對畫面。
  assert.equal(isPairingRequired(new ApiError(403, { error: 'non-local request' }, 'x')), false)
  assert.equal(isPairingRequired(new ApiError(401, { error: 'pairing_required' }, 'x')), false)
  assert.equal(isPairingRequired(new Error('boom')), false)
})

test('429 的文案帶還要等幾秒', () => {
  const soon = new ApiError(429, { error: 'pairing_rate_limited', retry_after_secs: 45 }, 'x')
  assert.equal(pairRetryAfterSecs(soon), 45)
  assert.equal(pairErrorText(soon), '猜太多次了，請等 45 秒後再試。')

  // daemon 鎖十分鐘：講「600 秒」沒人讀得動，拆成分秒。
  const locked = new ApiError(429, { error: 'pairing_rate_limited', retry_after_secs: 600 }, 'x')
  assert.equal(waitText(600), '10 分 0 秒')
  assert.equal(pairErrorText(locked), '猜太多次了，請等 10 分 0 秒後再試。')
  assert.equal(waitText(90), '1 分 30 秒')

  // 欄位缺了還是要是限流，只是說不出秒數。
  const noSecs = new ApiError(429, { error: 'pairing_rate_limited' }, 'x')
  assert.equal(pairRetryAfterSecs(noSecs), 0)
  assert.equal(pairErrorText(noSecs), '猜太多次了，請稍後再試。')
})

test('碼不對／過期／用過都是同一句，不透露是哪一種', () => {
  const failed = new ApiError(403, { error: 'pairing_failed', message: '配對碼不正確或已失效，請重新產生一個。' }, 'x')
  assert.match(pairErrorText(failed), /碼不正確或已失效/)
  assert.equal(pairRetryAfterSecs(failed), null)
})

test('在手機上按產碼會被擋，要說清楚只有本機產得出來', () => {
  const remote = new ApiError(403, { error: 'loopback_only' }, 'x')
  assert.match(pairCodeErrorText(remote), /只有在這台機器上的瀏覽器才產得出來/)
  // 其他錯誤照原訊息，不要被這句蓋掉。
  assert.equal(pairCodeErrorText(new Error('daemon 掛了')), 'daemon 掛了')
})
