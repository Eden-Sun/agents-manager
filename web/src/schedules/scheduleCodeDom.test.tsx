/**
 * 排程代號（S1、S2…）：列表每列、編輯器標題與「最近執行」、通知 toast 都帶代號（舊 daemon 沒有 code 就不顯示）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
import { ScheduleEditor } from './ScheduleEditor'
import { ProjectSchedules } from './ProjectSchedules'

afterEach(unmountAll)
let tzList: ReturnType<typeof shortTimeZoneList>
before(async () => {
  await setupDom()
  tzList = shortTimeZoneList()
})
after(async () => {
  tzList.restore()
  await teardownDom()
})

const row = (over: Record<string, unknown> = {}) => ({
  id: 's1', code: 'S12', project_id: 'p1', bot_id: 'b1', name: '每日', prompt: '整理進度', mode: 'daily', tz: 'UTC',
  spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure',
  clear_after: false, from_base: false, created_at: '', updated_at: '', open: null, recent: [], ...over,
})

test('編輯器標題與最近執行帶代號', async () => {
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return row()
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const text = document.body.textContent ?? ''
  assert.match(text, /S12/)
  assert.ok(document.body.querySelector('.schedule-recent summary')?.textContent?.startsWith('S12 '))
})

test('舊 daemon 沒有 code：不顯示 undefined', async () => {
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return row({ code: undefined })
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  assert.doesNotMatch(document.body.textContent ?? '', /undefined/)
})

test('專案排程列表每列帶代號', async () => {
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [row(), row({ id: 's2', code: 'S13', name: '週報' })] }
    return undefined
  })
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  const codes = [...document.body.querySelectorAll('.project-schedule-code')].map((n) => n.textContent?.trim())
  assert.deepEqual(codes, ['S12', 'S13'])
})
