import test from 'node:test'
import assert from 'node:assert/strict'
import { cadenceText, occurrenceLine, scheduleRowLines, whenText } from './format'
import { defaultTimeZone } from './format'

test('only a queued turn waiting for startup displays its original startup error', () => {
  const o = { status: 'queued' as const, turn_status: 'queued', turn_awaits_start: 1, turn_start_error: 'AM_GROK_UNTRUSTED' }
  assert.match(occurrenceLine(o), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine(o, 'in_flight'), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine({ ...o, turn_awaits_start: 0 }), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine({ ...o, turn_start_error: null }), /AM_GROK_UNTRUSTED/)
})

test('側欄短時間：同時區只有月日時分；不同時區補縮寫', async () => {
  const { formatDueShort, defaultTimeZone } = await import('./format')
  const iso = '2026-11-02T00:33:00Z'
  const here = defaultTimeZone()
  const short = formatDueShort(iso, here)
  assert.doesNotMatch(short, /2026/)
  assert.match(short, /^\d\d\/\d\d \d\d:\d\d$/)
  const other = here === 'Asia/Taipei' ? 'UTC' : 'Asia/Taipei'
  assert.notEqual(formatDueShort(iso, other), formatDueShort(iso, here))
})

test('列上的日期：今天／明天／昨天；7 天內帶星期；更遠只到月日；跨年加年；24 小時制', () => {
  const now = new Date('2026-11-12T04:00:00Z') // 週四
  const here = defaultTimeZone()
  // 排程時區＝瀏覽器時區時不補縮寫；不同時區時補上縮寫（UTC 的 short name 是 UTC）。
  const tag = here === 'UTC' ? '' : ' UTC'
  assert.equal(whenText('2026-11-12T09:30:00Z', 'UTC', now), `今天 09:30${tag}`)
  assert.equal(whenText('2026-11-13T09:30:00Z', 'UTC', now), `明天 09:30${tag}`)
  assert.equal(whenText('2026-11-11T09:30:00Z', 'UTC', now), `昨天 09:30${tag}`)
  assert.equal(whenText('2026-11-12T13:05:00Z', 'UTC', now), `今天 13:05${tag}`)
  assert.equal(whenText('2026-11-16T09:30:00Z', 'UTC', now), `11/16（一） 09:30${tag}`)
  assert.equal(whenText('2026-11-25T09:30:00Z', 'UTC', now), `11/25 09:30${tag}`)
  assert.equal(whenText('2027-01-05T09:30:00Z', 'UTC', now), `2027/01/05 09:30${tag}`)
})

test('列上的日期：排程時區跟瀏覽器不同才補縮寫（時間仍是排程時區的牆上時間）', () => {
  const now = new Date('2026-11-12T04:00:00Z')
  const here = defaultTimeZone()
  if (here === 'Asia/Taipei') return
  // 台北 17:30 ＝ UTC 09:30；同一瞬間用台北時區顯示，時間是 17:30 且補了縮寫。
  assert.equal(whenText('2026-11-12T09:30:00Z', 'Asia/Taipei', now).startsWith('今天 17:30'), true)
  assert.notEqual(whenText('2026-11-12T09:30:00Z', 'Asia/Taipei', now), whenText('2026-11-12T09:30:00Z', here, now))
})

test('列的三行：停用空心、原訂描述；隨機區間寫（隨機）；一次性跑完沒有下一次', () => {
  const now = new Date('2026-11-12T04:00:00Z').getTime()
  const base = { id: 's', project_id: 'p', bot_id: 'b', name: 'n', prompt: 'x', tz: 'UTC', generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', open: null, last: null } as const
  const daily = { ...base, mode: 'daily', spec: { mode: 'daily', time: '18:01', slots: [{ time: '18:01', until: '18:05' }] } }
  const off = scheduleRowLines({ ...daily, enabled: false, next_due: null } as never, undefined, now)
  assert.equal(off.dot, 'off')
  assert.equal(off.next.text, '已停用 · 原訂每天 18:01–18:05')
  assert.equal(cadenceText({ ...daily, enabled: false } as never), '每天 18:01–18:05')
  const random = scheduleRowLines({ ...daily, enabled: true, next_due: '2026-11-12T18:03:00Z' } as never, undefined, now)
  assert.match(random.next.text, /^今天 18:03/)
  assert.match(random.next.text, /（隨機）$/)
  assert.match(random.next.title, /隨機區間 18:01–18:05/)
  const once = scheduleRowLines({ ...base, mode: 'once', spec: { mode: 'once', at: '2026-11-12T10:00' }, enabled: true, next_due: null } as never, undefined, now)
  assert.equal(once.next.text, '已完成，沒有下一次')
  assert.equal(once.dot, 'never')
})
