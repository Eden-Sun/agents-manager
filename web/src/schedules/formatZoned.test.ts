/**
 * #1812：後端（chrono-tz）接受的合法時區，瀏覽器 Intl 可能不認識。這時格式化不能丟 RangeError 把排程畫面炸掉，
 * 要退回 UTC 顯示，並在字串裡明講這台瀏覽器不支援、原時區名保留。
 * 不靠真的 Intl 資料版本：用假的 `toLocaleString` 模擬「不認識這個時區」。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { formatDue, formatZoned } from './format.ts'

const ISO = '2030-01-01T00:00:00.000Z'

/**
 * 只有 `known` 裡的時區能格式化（實際排版交給真的 Intl，但換成 UTC，避免依賴這台機器的 tzdb）；
 * 其餘丟 RangeError，跟瀏覽器對未知時區的行為一樣。
 */
function withUnknownZones(known: string[], fn: () => void): void {
  const original = Date.prototype.toLocaleString
  Date.prototype.toLocaleString = function (this: Date, locale?: string | string[], options?: Intl.DateTimeFormatOptions) {
    const tz = options?.timeZone
    if (tz && !known.includes(tz)) throw new RangeError(`Invalid time zone specified: ${tz}`)
    return original.call(this, locale, { ...options, timeZone: tz ? 'UTC' : undefined })
  }
  try {
    fn()
  } finally {
    Date.prototype.toLocaleString = original
  }
}

test('不認識的合法時區：不丟例外，以 UTC 顯示，並標出原時區與不支援', () => {
  withUnknownZones(['UTC'], () => {
    const out = formatZoned(ISO, 'America/Coyhaique')
    assert.match(out, /America\/Coyhaique/, '原時區名要保留')
    assert.match(out, /不支援/, '要明示瀏覽器不支援此時區')
    assert.match(out, /UTC/, '要標出是以 UTC 顯示')
  })
})

test('排程列的「下次」也走同一條 fallback：formatDue 不炸', () => {
  withUnknownZones(['UTC'], () => {
    const out = formatDue(ISO, 'America/Coyhaique')
    assert.match(out, /America\/Coyhaique/)
    assert.match(out, /不支援/)
  })
})

test('認識的時區：輸出不變（時區名附在後面，沒有不支援的標示）', () => {
  withUnknownZones(['UTC', 'America/Coyhaique'], () => {
    const out = formatZoned(ISO, 'America/Coyhaique')
    assert.ok(out.endsWith('（America/Coyhaique）'), out)
    assert.doesNotMatch(out, /不支援/)
  })
})

test('一般時區：格式與 formatDue 相同（沒有 fallback 標示）', () => {
  const out = formatDue(ISO, 'UTC')
  assert.ok(out.endsWith('（UTC）'), out)
  assert.doesNotMatch(out, /不支援/)
})

test('無效日期：原字串原樣回傳，不丟例外', () => {
  assert.equal(formatZoned('not-a-date', 'UTC'), 'not-a-date')
})

test('沒有下一次：維持原本的文字', () => {
  assert.equal(formatDue(null, 'UTC'), '沒有下一次（已完成或已停用）')
})
