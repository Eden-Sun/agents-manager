import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import type { ScheduleDetail } from '../api/schedules'
import { useStore } from '../store/store'
import { ProjectSchedules } from './ProjectSchedules'
import { randomSummary, scheduleStatusLine, scheduleStatusShort } from './format'

before(setupDom)
after(async () => {
  for (const notice of useStore.getState().notices) useStore.getState().dismiss(notice.id)
  await teardownDom()
})
afterEach(unmountAll)
beforeEach(() => {
  useStore.setState({
    projects: [{ id: 'p1', label: '專案', path: '/project', host: 'local' }],
    bots: [{ id: 'b1', name: '原本的 Bot', project_id: 'p1', kind: 'claude', role: 'scheduler', model: null, effort: null, managed_by: 'user', parent_bot_id: null, share_profile: null }],
    runs: {}, notices: [], schedulesRev: 0,
  } as never)
})

const menuItem = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent?.trim() === text)!
const dialog = () => document.querySelector<HTMLElement>('[role="dialog"]')!
const buttonIn = (root: ParentNode, text: string) => [...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text)!

function source(slots: unknown[] = [{ time: '08:55', label: '', until: '09:00' }]): ScheduleDetail {
  return {
    id: 'source', project_id: 'p1', bot_id: 'b1', name: '打卡', prompt: '打卡', mode: 'daily',
    spec: { mode: 'daily', time: '08:55', slots },
    tz: 'UTC', notify: 'all', clear_after: false, retry_count: 0, retry_interval_mins: 3, retry_deadline_mins: 15,
    enabled: true, generation: 8, next_due: '2030-01-01T08:57:12Z', created_at: '', updated_at: '',
    open: null, recent: [],
  } as unknown as ScheduleDetail
}

async function openCopy(requests: ReturnType<typeof fakeApi>, detail = source()) {
  void requests
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!)
  await click(menuItem('複製排程…'))
  await settle()
  return detail
}

test('隨機區間：摘要顯示「08:55–09:00 隨機」，列表與狀態列帶下一次抽到的秒', () => {
  const s = source()
  assert.equal(randomSummary(s.spec), '08:55–09:00 隨機')
  assert.equal(randomSummary({ mode: 'daily', time: '09:00' }), '', '沒有區間不顯示')
  assert.match(scheduleStatusLine(s), /08:55–09:00 隨機 · .*08:57:12/)
  assert.match(scheduleStatusShort(s), /08:55–09:00 隨機 · 01\/01 08:57:12/)
  const fixed = { ...s, spec: { mode: 'daily', time: '09:00' } }
  assert.doesNotMatch(scheduleStatusShort(fixed), /隨機/)
  assert.doesNotMatch(scheduleStatusShort(fixed), /:\d\d:\d\d/, '固定時間不顯示秒')
})

test('隨機區間：開關打開才出現結束時間，送出帶 until；關掉就回固定時間', async () => {
  const fixedSource = source([{ time: '09:00', label: '' }])
  const requests = fakeApi((r) => {
    if (r.path.endsWith('/projects/p1/schedules') && r.method === 'GET') return { schedules: [fixedSource] }
    if (r.path.endsWith('/schedules/source')) return fixedSource
    if (r.method === 'POST') return { ...fixedSource, ...(r.body as object), id: 'copy' }
  })
  await openCopy(requests)
  const toggle = () => dialog().querySelector<HTMLInputElement>('input[role=switch]')!
  assert.equal(toggle().checked, false)
  assert.equal(dialog().querySelector('.schedule-slot-until'), null, '關著時沒有結束時間欄')

  await click(toggle())
  assert.equal(toggle().checked, true)
  const until = () => dialog().querySelector<HTMLInputElement>('.schedule-slot-until input[type=time]')!
  assert.equal(until().value, '09:05', '預設結束＝開始＋5 分鐘')
  assert.match(dialog().querySelector('.schedule-slot-range')!.textContent ?? '', /09:00–09:05 隨機/)

  await typeInto(until(), '08:00')
  assert.match(dialog().querySelector('.schedule-slot-range [role=alert]')?.textContent ?? '', /結束時間要晚於開始時間/)
  await click(buttonIn(dialog(), '建立排程'))
  await settle()
  assert.equal(requests.filter((r) => r.method === 'POST').length, 0, '結束早於開始不送出')

  await typeInto(until(), '09:10')
  await click(buttonIn(dialog(), '建立排程'))
  await settle()
  const posts = requests.filter((r) => r.method === 'POST')
  assert.equal(posts.length, 1)
  assert.deepEqual((posts[0].body as { spec: unknown }).spec, { mode: 'daily', time: '09:00', slots: [{ time: '09:00', until: '09:10' }] })
})

test('隨機區間：超過 12 小時擋下；關掉開關後 spec 不帶 until', async () => {
  const requests = fakeApi((r) => {
    if (r.path.endsWith('/projects/p1/schedules') && r.method === 'GET') return { schedules: [source()] }
    if (r.path.endsWith('/schedules/source')) return source()
    if (r.method === 'POST') return { ...source(), ...(r.body as object), id: 'copy' }
  })
  await openCopy(requests)
  const toggle = () => dialog().querySelector<HTMLInputElement>('input[role=switch]')!
  assert.equal(toggle().checked, true, '既有區間讀進來是打開的')
  assert.equal(dialog().querySelector<HTMLInputElement>('.schedule-slot-until input')!.value, '09:00')
  await typeInto(dialog().querySelector<HTMLInputElement>('.schedule-slot-until input')!, '23:00')
  assert.match(dialog().querySelector('.schedule-slot-range [role=alert]')?.textContent ?? '', /最長 12 小時/)

  await click(toggle())
  assert.equal(dialog().querySelector('.schedule-slot-until'), null)
  await click(buttonIn(dialog(), '建立排程'))
  await settle()
  const posts = requests.filter((r) => r.method === 'POST')
  assert.equal(posts.length, 1)
  assert.deepEqual((posts[0].body as { spec: unknown }).spec, { mode: 'daily', time: '08:55' })
})
