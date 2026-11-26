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
  const now = new Date('2026-11-12T04:00:00Z')
  const here = defaultTimeZone()
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

test('狀態點：上次失敗＋現在等待 → 黃（waiting）；上次失敗＋沒有未結束的 occurrence → 紅（fail）', () => {
  const now = new Date('2026-11-12T04:00:00Z').getTime()
  const base = { id: 's', project_id: 'p', bot_id: 'b', name: 'n', prompt: 'x', tz: 'UTC', generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', mode: 'daily', spec: { mode: 'daily', time: '18:01' }, enabled: true, next_due: '2026-11-12T18:01:00Z' } as const
  const failedLast: Record<string, unknown> = { status: 'failed', outcome: 'failure', started_at: '2026-11-11T18:01:00Z', updated_at: '2026-11-11T18:02:00Z', duration_ms: 1000 }
  const pending: Record<string, unknown> = { id: 'o1', schedule_id: 's', source: 'scheduled', scheduled_for: '2026-11-12T18:01:00Z', generation: 1, bot_id: 'b', prompt: 'x', status: 'pending', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null }
  const waiting = scheduleRowLines({ ...base, open: pending, last: failedLast } as never, undefined, now)
  assert.equal(waiting.next.label, '現在')
  assert.equal(waiting.next.text, '等待送出')
  assert.equal(waiting.dot, 'waiting', '現在是等待，就算上次失敗也是黃')
  const retrying = { ...pending, status: 'pending', next_retry_at: '2026-11-12T04:30:00Z', attempt_no: 2 }
  assert.equal(scheduleRowLines({ ...base, open: retrying, last: failedLast } as never, undefined, now).dot, 'waiting', '等待重試也是黃')
  const queued = { ...pending, status: 'queued', turn_status: 'queued' }
  assert.equal(scheduleRowLines({ ...base, open: queued, last: failedLast } as never, undefined, now).dot, 'waiting', 'queued 未開始是黃')
  const idle = scheduleRowLines({ ...base, open: null, last: failedLast } as never, undefined, now)
  assert.equal(idle.dot, 'fail', '沒有未結束的 occurrence 才看上次結果：紅')
})

test('停用但仍有執行中的 occurrence：下次行顯示執行中（附已停用），狀態點不變成灰；沒有 occurrence 才顯示已停用', () => {
  const now = new Date('2026-11-12T04:00:00Z').getTime()
  const base = { id: 's', project_id: 'p', bot_id: 'b', name: 'n', prompt: 'x', tz: 'UTC', generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', mode: 'daily', spec: { mode: 'daily', time: '18:01' }, enabled: false, next_due: null } as const
  const failedLast: Record<string, unknown> = { status: 'failed', outcome: 'failure', started_at: '2026-11-11T18:01:00Z', updated_at: '2026-11-11T18:02:00Z', duration_ms: 1000 }
  const running: Record<string, unknown> = { id: 'o1', schedule_id: 's', source: 'scheduled', scheduled_for: '2026-11-12T03:00:00Z', generation: 1, bot_id: 'b', prompt: 'x', status: 'queued', waiting_reason: null, turn_status: 'in_flight', running_since: '2026-11-12T03:59:00Z', merged_count: 0, conversation_id: null, turn_id: null }
  const r = scheduleRowLines({ ...base, open: running, last: failedLast } as never, undefined, now)
  assert.equal(r.next.label, '現在')
  assert.match(r.next.text, /^執行中 · 已 .+ · 已停用$/)
  assert.notEqual(r.dot, 'off', '執行中的停用排程不變灰')
  assert.equal(r.dot, 'fail', '沿用上次結果的顏色，與同一顆啟用時相同')
  const pending: Record<string, unknown> = { ...running, status: 'pending', turn_status: null, running_since: null }
  const waiting = scheduleRowLines({ ...base, open: pending, last: failedLast } as never, undefined, now)
  assert.equal(waiting.next.text, '等待送出 · 已停用')
  assert.equal(waiting.dot, 'waiting')
  const idle = scheduleRowLines({ ...base, open: null, last: failedLast } as never, undefined, now)
  assert.match(idle.next.text, /^已停用 · 原訂/, '沒有 occurrence 才是已停用')
  assert.equal(idle.dot, 'off')
})

test('狀態點：執行中沿用上次結果的顏色（不是黃）', () => {
  const now = new Date('2026-11-12T04:00:00Z').getTime()
  const base = { id: 's', project_id: 'p', bot_id: 'b', name: 'n', prompt: 'x', tz: 'UTC', generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', mode: 'daily', spec: { mode: 'daily', time: '18:01' }, enabled: true, next_due: '2026-11-12T18:01:00Z' } as const
  const failedLast: Record<string, unknown> = { status: 'failed', outcome: 'failure', started_at: '2026-11-11T18:01:00Z', updated_at: '2026-11-11T18:02:00Z', duration_ms: 1000 }
  const running: Record<string, unknown> = { id: 'o1', schedule_id: 's', source: 'scheduled', scheduled_for: '2026-11-12T03:00:00Z', generation: 1, bot_id: 'b', prompt: 'x', status: 'queued', waiting_reason: null, turn_status: 'in_flight', running_since: '2026-11-12T03:59:00Z', merged_count: 0, conversation_id: null, turn_id: null }
  const r = scheduleRowLines({ ...base, open: running, last: failedLast } as never, undefined, now)
  assert.equal(r.next.label, '現在')
  assert.match(r.next.text, /^執行中/)
  assert.equal(r.dot, 'fail', '執行中不上黃，沿用上次結果')
  const live = scheduleRowLines({ ...base, open: { ...running, running_since: null, turn_status: 'queued' }, last: failedLast } as never, 'in_flight', now)
  assert.match(live.next.text, /^執行中/)
  assert.notEqual(live.dot, 'waiting', '即時狀態是 in_flight 也算執行中')
})
