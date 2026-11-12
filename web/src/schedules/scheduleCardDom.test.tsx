/**
 * 專案排程列（固定三行）的四種狀態＋執行中＋等待 Bot；假資料，不含真實任務內容。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ProjectSchedules } from './ProjectSchedules'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const soon = new Date(Date.now() + 3 * 3_600_000).toISOString()
const base = {
  project_id: 'p1', bot_id: 'b1', prompt: '假任務', tz: 'UTC', mode: 'daily' as const,
  spec: { mode: 'daily', time: '09:00' }, next_due: soon, enabled: true, generation: 1, notify: 'failure' as const,
  clear_after: false, from_base: false, created_at: '', updated_at: '', open: null,
}
const done = (outcome: string, status = 'done', extra: Record<string, unknown> = {}) => ({
  id: 'o', bot_id: 'b1', status, outcome, scheduled_for: '', generation: 1, source: 'scheduled', prompt: '', merged_count: 0, conversation_id: null, turn_id: null, error: null,
  started_at: '2026-11-11T01:02:00Z', updated_at: '2026-11-11T01:02:40Z', duration_ms: 40_000, created_at: '', ...extra,
})

async function rows(schedules: unknown[]) {
  fakeApi(() => ({ schedules }))
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  return [...document.querySelectorAll<HTMLElement>('.project-schedule-item')]
}

test('四種狀態：成功（綠）／失敗（紅）／從未執行（灰）／停用（空心、整張淡化）', async () => {
  const [ok, fail, never, off] = await rows([
    { ...base, id: 'a', code: 'S1', name: '成功', last: done('success') },
    { ...base, id: 'b', code: 'S2', name: '失敗', last: done('failed', 'failed') },
    { ...base, id: 'c', code: 'S3', name: '新排程', last: null },
    { ...base, id: 'd', code: 'S4', name: '停用的', enabled: false, spec: { mode: 'daily', time: '18:01', slots: [{ time: '18:01', until: '18:05' }] } },
  ])
  assert.ok(ok.querySelector('.project-schedule-dot.ok'))
  assert.match(ok.textContent ?? '', /✓ 成功 · .* · 花 40 秒/)
  assert.ok(fail.querySelector('.project-schedule-dot.fail'))
  assert.match(fail.textContent ?? '', /✕ 失敗/)
  assert.ok(never.querySelector('.project-schedule-dot.never'))
  assert.match(never.textContent ?? '', /上次還沒執行過/)
  assert.ok(off.classList.contains('off'), '停用整張淡化')
  assert.ok(off.querySelector('.project-schedule-dot.off'), '停用＝空心點')
  assert.match(off.textContent ?? '', /已停用 · 原訂每天 18:01–18:05/)
})

test('執行中：現在一行顯示已進行時間；等待 Bot 忙碌中顯示等待', async () => {
  const since = new Date(Date.now() - 25_000).toISOString()
  const [running, waiting] = await rows([
    { ...base, id: 'r', code: 'S5', name: '執行中', last: null, open: { ...done('', 'queued'), outcome: undefined, status: 'queued', turn_status: 'in_flight', running_since: since, duration_ms: 0, turn_id: 't1' } },
    { ...base, id: 'w', code: 'S6', name: '等待', last: null, open: { ...done('', 'pending'), outcome: undefined, status: 'pending', waiting_reason: 'bot_busy', turn_id: null } },
  ])
  assert.match(running.textContent ?? '', /現在執行中 · 已 2[4-7] 秒/)
  assert.match(waiting.textContent ?? '', /現在等待 Bot（忙碌中）/)
})

test('一次性跑完：沒有下一次；Bot 不可用維持原本的重新指定文案', async () => {
  const [once, unbound] = await rows([
    { ...base, id: 'o', code: 'S7', name: '一次', mode: 'once', spec: { mode: 'once', at: '2026-11-11T10:00' }, next_due: null, last: done('success') },
    { ...base, id: 'u', code: 'S8', name: '失效', bot_available: false, last: null },
  ])
  assert.match(once.textContent ?? '', /已完成，沒有下一次/)
  assert.match(unbound.textContent ?? '', /⚠ 需要重新指定 Bot/)
})

test('日期格式：今天、明天、昨天、7 天內帶星期、跨年加年；列的 title 放完整時區', async () => {
  const [row] = await rows([{ ...base, id: 'x', code: 'S9', name: '日期', next_due: '2027-01-05T09:30:00Z', last: null }])
  assert.match(row.textContent ?? '', /下次2027\/01\/05 09:30/)
  assert.ok(row.querySelector('.project-schedule-fact[title]')?.getAttribute('title')?.includes('2027'), 'tooltip 放完整日期時間')
})
