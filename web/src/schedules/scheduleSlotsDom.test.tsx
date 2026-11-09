import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import type { ScheduleDetail } from '../api/schedules'
import { useStore } from '../store/store'
import { ProjectSchedules } from './ProjectSchedules'

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

function source(): ScheduleDetail {
  return {
    id: 'source', project_id: 'p1', bot_id: 'b1', name: '打卡', prompt: '{{slot}} 打卡', mode: 'daily',
    spec: { mode: 'daily', time: '08:30', slots: [{ time: '08:30', label: '上班' }, { time: '18:05', label: '下班' }] },
    tz: 'Asia/Taipei', notify: 'all', clear_after: true, retry_count: 0, retry_interval_mins: 3, retry_deadline_mins: 15,
    enabled: false, generation: 8, next_due: '2030-01-01T00:00:00Z', created_at: '', updated_at: '',
    open: null, recent: [],
  } as unknown as ScheduleDetail
}

test('多時段：列出、增刪時段，送出帶 slots；變數點了插入、預覽與未知變數提示', async () => {
  const requests = fakeApi((r) => {
    if (r.path.endsWith('/projects/p1/schedules') && r.method === 'GET') return { schedules: [source()] }
    if (r.path.endsWith('/schedules/source')) return source()
    if (r.method === 'POST') return { ...source(), ...(r.body as object), id: 'copy', enabled: true, generation: 1 }
  })
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!)
  await click(menuItem('複製排程…'))
  await settle()

  const rows = () => dialog().querySelectorAll('.schedule-slot-row')
  assert.equal(rows().length, 2, '兩個時段各一列')
  assert.equal(rows()[1].querySelector<HTMLInputElement>('input[type=text]')!.value, '下班')

  await click(buttonIn(dialog(), '新增時段'))
  assert.equal(rows().length, 3)
  await click(buttonIn(rows()[1] as HTMLElement, '刪除時段'))
  assert.equal(rows().length, 2, '刪掉的是第二列（下班）')

  const prompt = dialog().querySelector<HTMLTextAreaElement>('textarea')!
  await click(buttonIn(dialog(), '{{weekday}}'))
  assert.match(prompt.value, /\{\{weekday\}\}/, '點變數會插入 token')

  await click(buttonIn(dialog(), '預覽展開結果'))
  const preview = dialog().querySelector('.schedule-preview')!.textContent ?? ''
  assert.doesNotMatch(preview, /\{\{/, '預覽已展開已知變數')
  assert.match(preview, /上班/, '預覽的時段標籤是第一個時段')

  await typeInto(prompt, '{{slot}} {{foo}}')
  assert.match(dialog().textContent ?? '', /不認得的變數 \{\{foo\}\} 會原樣保留/)

  await click(buttonIn(dialog(), '建立排程'))
  await settle()
  const posts = requests.filter((r) => r.method === 'POST')
  assert.equal(posts.length, 1)
  const body = posts[0].body as { spec: unknown }
  assert.deepEqual(body.spec, {
    mode: 'daily', time: '08:30',
    slots: [{ time: '08:30', label: '上班' }, { time: '09:00', label: '' }],
  })
})
