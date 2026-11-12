/**
 * 排程清單與編輯器要把下次觸發、上次結果、花費時間分成可掃的區塊（假資料，不含真實任務內容）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ProjectSchedules } from './ProjectSchedules'
import { ScheduleEditor } from './ScheduleEditor'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const base = {
  project_id: 'p1', bot_id: 'b1', prompt: '假任務', tz: 'UTC', mode: 'daily' as const,
  spec: { mode: 'daily', time: '09:00', slots: [{ time: '09:00', until: '09:05' }] },
  next_due: '2030-01-02T01:00:00.000Z', enabled: true, generation: 1, notify: 'failure' as const,
  clear_after: false, from_base: false, created_at: '', updated_at: '', open: null,
}

test('清單卡片：固定三行（名稱／下次／上次），沒有上次顯示還沒執行過，不再有時鐘圖示', async () => {
  fakeApi(() => ({ schedules: [
    { ...base, id: 's1', code: 'S1', name: '示範一', last: { id: 'o1', bot_id: 'b1', status: 'done', outcome: 'success', duration_ms: 134_000 } },
    { ...base, id: 's2', code: 'S2', name: '示範二', last: null },
  ] }))
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  const rows = [...document.querySelectorAll('.project-schedule-item')]
  assert.match(rows[0].textContent ?? '', /下次/)
  assert.match(rows[0].textContent ?? '', /✓ 成功/)
  assert.match(rows[0].textContent ?? '', /花 2 分 14 秒/)
  assert.ok(rows[0].querySelector('.project-schedule-dot.ok'), '上次成功＝綠點')
  assert.equal(rows[0].querySelector('.project-schedule-mark'), null, '不再畫時鐘圖示')
  assert.match(rows[1].textContent ?? '', /下次/)
  assert.match(rows[1].textContent ?? '', /還沒執行過/)
  assert.ok(rows[1].querySelector('.project-schedule-dot.never'), '從未執行＝灰點')
})

test('編輯器三格：下次、狀態、上次花費時間，格內只留值不重複標籤，不重複已儲存 Bot', async () => {
  const detail = {
    ...base, id: 's1', code: 'S1', name: '示範一', bot_dedicated: true,
    last: { id: 'o1', bot_id: 'b1', status: 'done', outcome: 'failed', duration_ms: 134_000 },
    recent: [],
  }
  fakeApi((req) => (req.method === 'GET' && req.path.endsWith('/schedules/s1') ? detail : { schedules: [] }))
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const glance = document.querySelector('.schedule-glance')
  assert.ok(glance)
  assert.match(glance?.textContent ?? '', /下次觸發/)
  assert.doesNotMatch(glance?.textContent ?? '', /下次：/)
  assert.match(glance?.textContent ?? '', /啟用中/)
  assert.doesNotMatch(glance?.textContent ?? '', /狀態：/)
  assert.match(glance?.textContent ?? '', /上次結果/)
  assert.match(glance?.textContent ?? '', /任務失敗/)
  assert.match(glance?.textContent ?? '', /上次 2 分 14 秒/)
  assert.doesNotMatch(document.body.textContent ?? '', /已儲存的 Bot：/)
  const details = document.querySelector('details.schedule-recent')
  assert.equal(details?.getAttribute('open'), null)
  assert.match(details?.querySelector('summary')?.textContent ?? '', /S1 最近執行（0）/)
})
