import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import type { PairCode } from '../api/types.ts'
import {
  expiryText,
  formatPairCode,
  isPairingRequired,
  normalizePairCode,
  pairCodeErrorText,
  pairCodeSecsLeft,
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

test('429 把 retry_after_secs 換算成白話，不把秒數原樣丟出去', () => {
  // 不到一分鐘才講秒。
  const soon = new ApiError(429, { error: 'pairing_rate_limited', retry_after_secs: 45 }, 'x')
  assert.equal(pairRetryAfterSecs(soon), 45)
  assert.equal(waitText(45), '45 秒')
  assert.equal(pairErrorText(soon), '猜太多次了，請 45 秒後再試。')

  // daemon 鎖十分鐘：「請等 600 秒」沒人讀得動。
  const locked = new ApiError(429, { error: 'pairing_rate_limited', retry_after_secs: 600 }, 'x')
  assert.equal(waitText(600), '10 分鐘')
  assert.equal(pairErrorText(locked), '猜太多次了，請 10 分鐘後再試。')
  // 無條件進位：寧可多等一下，也不要叫人時間還沒到就白按一次。
  assert.equal(waitText(61), '2 分鐘')
  assert.equal(waitText(90), '2 分鐘')

  // 欄位缺了還是要算限流，只是說不出還要多久。
  const noSecs = new ApiError(429, { error: 'pairing_rate_limited' }, 'x')
  assert.equal(pairRetryAfterSecs(noSecs), 0)
  assert.equal(pairErrorText(noSecs), '猜太多次了，請稍後再試。')
})

test('產出來的碼：倒數看得到秒在動，到期就歸零', () => {
  const issued = Date.parse('2026-09-16T12:00:00.000Z')
  const info: PairCode = {
    code: 'ABC-DEF',
    expires_in_secs: 300,
    expires_at: '2026-09-16T12:05:00Z',
  }

  assert.equal(pairCodeSecsLeft(info, issued, issued), 300)
  // 倒數用分秒：寫「5 分鐘」的話整整五分鐘都不會變，看不出還來不來得及。
  assert.equal(expiryText(300), '5 分 0 秒')
  assert.equal(expiryText(pairCodeSecsLeft(info, issued, issued + 1_000)), '4 分 59 秒')
  assert.equal(expiryText(pairCodeSecsLeft(info, issued, issued + 250_000)), '50 秒')

  // 到期之後不會變負數，畫面據此改口說「再產生一個」。
  assert.equal(pairCodeSecsLeft(info, issued, issued + 300_000), 0)
  assert.equal(pairCodeSecsLeft(info, issued, issued + 999_000), 0)

  // 以 `expires_at` 為準，不是「拿到碼的時刻 + 300」：回應晚到（或本機時鐘偏了）時兩者會差一截，
  // 自己算出來的那個會讓已經快過期的碼看起來還很新。
  const late: PairCode = { code: 'ABC-DEF', expires_in_secs: 300, expires_at: '2026-09-16T12:05:00Z' }
  assert.equal(pairCodeSecsLeft(late, issued + 30_000, issued + 30_000), 270)

  // daemon 沒給 `expires_at` 才退回發碼當下的秒數。
  const stale: PairCode = { code: 'ABC-DEF', expires_in_secs: 300, expires_at: null }
  assert.equal(pairCodeSecsLeft(stale, issued, issued + 120_000), 180)
  const broken: PairCode = { code: 'ABC-DEF', expires_in_secs: 300, expires_at: 'not-a-date' }
  assert.equal(pairCodeSecsLeft(broken, issued, issued + 120_000), 180)
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
