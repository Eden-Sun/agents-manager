/**
 * 排程專屬 bot：編輯器只列專屬 bot、「＋ 建立專屬 bot」預設名稱用排程名稱並以 role=scheduler 建立；
 * 既有排程綁一般 bot 時照常顯示、提示並可「轉成專屬」；側欄把專屬 bot 收進預設收合的「排程 bot」區。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
import { useStore } from '../store/store'
import { Sidebar } from '../components/Sidebar'
import { ScheduleEditor } from './ScheduleEditor'

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

const bot = (id: string, over: Record<string, unknown> = {}) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', role: null, identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0,
  cwd: null, share_profile: null, ...over,
})

beforeEach(() => {
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local', mode: 'parent_child' }],
    bots: [bot('s1', { role: 'scheduler' }), bot('s2', { role: 'scheduler' }), bot('w1', { role: 'executor' }), bot('g1')],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 's1', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

const row = (over: Record<string, unknown> = {}) => ({
  id: 'x1', project_id: 'p1', bot_id: 's1', bot_available: true, bot_dedicated: true, name: '網站上班打卡', prompt: 'x', mode: 'daily', tz: 'UTC',
  spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure',
  clear_after: false, created_at: '', updated_at: '', open: null, recent: [], ...over,
})

test('新增排程：只列本專案的排程專屬 bot（不列執行者與一般 bot）', async () => {
  fakeApi((req) => (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules') ? { schedules: [] } : { locked: false }))
  await mount(<ScheduleEditor projectId="p1" scheduleId={null} onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const listed = [...document.body.querySelectorAll<HTMLElement>('.sched-bot-row')].map((e) => e.dataset.botId).sort()
  assert.deepEqual(listed, ['s1', 's2'])
  assert.equal(document.body.querySelector<HTMLElement>('.sched-bot-row[aria-pressed="true"]')?.dataset.botId, 's1', '預設選第一顆專屬 bot')
})

test('「＋ 建立專屬 bot」：預設名稱用排程名稱，以 role=scheduler 建立並自動選上', async () => {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [] }
    if (req.method === 'POST' && req.path.endsWith('/projects/p1/bots')) return { bot_id: 'n1', name: (req.body as { name: string }).name }
    return { locked: false }
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId={null} onClose={() => {}} onSelect={() => {}} />)
  await settle()
  await typeInto(document.body.querySelector<HTMLInputElement>('.schedule-editor input')!, '網站上班')
  await click([...document.body.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '＋ 建立專屬 bot')!)
  await settle()
  const dialog = [...document.querySelectorAll<HTMLElement>('[role="dialog"]')].find((d) => d.textContent?.includes('建立排程專屬 bot'))!
  assert.equal(dialog.querySelector<HTMLInputElement>('input[type="text"]')!.value, '網站上班')
  await click([...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.getAttribute('type') === 'submit')!)
  await settle()
  const post = requests.find((r) => r.method === 'POST' && r.path.endsWith('/projects/p1/bots'))!
  assert.equal((post.body as { role?: string }).role, 'scheduler')
  assert.equal((post.body as { name: string }).name, '網站上班')
})

test('既有排程綁一般 bot：照常列出、提示，並可「轉成專屬」', async () => {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/x1')) return row({ bot_id: 'w1', bot_dedicated: false })
    if (req.method === 'PATCH' && req.path.endsWith('/bots/w1')) return { needs_restart: false }
    return { locked: false, schedules: [] }
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="x1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const listed = [...document.body.querySelectorAll<HTMLElement>('.sched-bot-row')].map((e) => e.dataset.botId).sort()
  assert.deepEqual(listed, ['s1', 's2', 'w1'], '綁著的一般 bot 照常列出')
  assert.match(document.body.textContent ?? '', /綁的不是排程專屬 bot/)
  await click([...document.body.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '轉成專屬')!)
  await settle()
  const patch = requests.find((r) => r.method === 'PATCH' && r.path.endsWith('/bots/w1'))!
  assert.deepEqual(patch.body, { role: 'scheduler' })
})

test('專屬 bot 綁定的排程沒有提示', async () => {
  fakeApi((req) => (req.method === 'GET' && req.path.endsWith('/schedules/x1') ? row() : { locked: false, schedules: [] }))
  await mount(<ScheduleEditor projectId="p1" scheduleId="x1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  assert.doesNotMatch(document.body.textContent ?? '', /綁的不是排程專屬 bot/)
  assert.ok(![...document.body.querySelectorAll('button')].some((b) => b.textContent?.trim() === '轉成專屬'))
})

test('側欄：排程專屬 bot 收進預設收合的「排程 bot」區，不混在其他 bot 裡', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  const ids = () => [...document.querySelectorAll<HTMLElement>('[data-bot-id]')].map((e) => e.dataset.botId)
  assert.ok(ids().includes('w1') && ids().includes('g1'))
  assert.ok(!ids().includes('s1') && !ids().includes('s2'), '預設收合：專屬 bot 不在清單裡')
  const head = document.querySelector<HTMLButtonElement>('.scheduler-bots-head')!
  assert.match(head.textContent ?? '', /排程 bot/)
  assert.equal(head.getAttribute('aria-expanded'), 'false')
  await click(head)
  assert.ok(ids().includes('s1') && ids().includes('s2'))
  assert.equal(head.getAttribute('aria-expanded'), 'true')
})
