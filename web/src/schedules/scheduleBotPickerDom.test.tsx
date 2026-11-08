/**
 * 排程編輯器手機版：Bot 清單（資訊、灰掉的原因、已被用的標示）、時間衝突提示、儲存列固定在底部。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
import { useStore } from '../store/store'
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
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: 'cc0', model: 'opus', effort: 'high', args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, role: 'executor',
  primary: false, primary_position: 0, cwd: null, share_profile: null, fast: false, persona: null, ...over,
})

beforeEach(() => {
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local' }],
    bots: [
      bot('b1'),
      bot('b2', { identity: 'cc1', role: 'advisor' }),
      bot('b3', { project_id: 'p2' }),
      bot('b4', { kind: 'claude-api' }),
      bot('b5', { managed_by: 'child', parent_bot_id: 'b1' }),
    ],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'b1', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

const row = (id: string) => document.querySelector<HTMLButtonElement>(`[data-bot-id="${id}"]`)!

const other = (over: Record<string, unknown> = {}) => ({
  id: 'o1', project_id: 'p1', bot_id: 'b2', bot_available: true, name: '早上彙整', prompt: 'x', mode: 'daily', tz: 'UTC',
  spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure',
  clear_after: false, created_at: '', updated_at: '', open: null, retry_count: 0, retry_interval_mins: 3, retry_deadline_mins: 15,
  ...over,
})

async function openNew(others: Array<Record<string, unknown>>) {
  fakeApi((req) => (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules') ? { schedules: others } : undefined))
  await mount(<ScheduleEditor projectId="p1" scheduleId={null} onClose={() => {}} onSelect={() => {}} />)
  await settle()
}

test('Bot 清單：每列有 kind、身份、角色；同專案一般 Bot 可選，別專案與 claude-api、child 灰掉並寫原因', async () => {
  await openNew([])
  assert.match(row('b1').textContent ?? '', /bot-b1/)
  assert.match(row('b1').textContent ?? '', /cc0/)
  assert.match(row('b2').textContent ?? '', /cc1/)
  assert.match(row('b2').textContent ?? '', /顧/)
  assert.equal(row('b1').getAttribute('aria-pressed'), 'true')
  assert.equal(row('b2').disabled, false)
  assert.equal(row('b3').disabled, true)
  assert.match(row('b3').textContent ?? '', /屬於其他專案/)
  assert.equal(row('b4').disabled, true)
  assert.match(row('b4').textContent ?? '', /claude-api 不能接排程/)
  assert.match(row('b5').textContent ?? '', /子 agent 不能接排程/)
})

test('Bot 清單：目前選的置頂；已被其他排程用的標「已有 N 個排程」', async () => {
  await openNew([other(), other({ id: 'o2', name: '晚上摘要', next_due: '2030-01-01T20:00:00.000Z', spec: { mode: 'daily', time: '20:00' } })])
  const list = [...document.querySelectorAll<HTMLElement>('.sched-bot-row')]
  assert.equal(list[0].dataset.botId, 'b1', '選的 Bot 置頂')
  assert.match(row('b2').textContent ?? '', /已有 2 個排程/)
  assert.doesNotMatch(row('b1').textContent ?? '', /已有/)
})

test('選了已被排程用、時間相近的 Bot：顯示同一顆 Bot 依序執行的提示；換一顆就消失', async () => {
  await openNew([other({ next_due: '2030-01-01T09:00:00.000Z' })])
  // 新排程的時刻表單預設不是 09:00；先把時間設成 09:05，再選 b2。
  const time = document.querySelector<HTMLInputElement>('.schedule-editor input[type="time"]')!
  assert.ok(time, '每天模式要有時間欄')
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(time, '09:05')
    time.dispatchEvent(new Event('input', { bubbles: true }))
    time.dispatchEvent(new Event('change', { bubbles: true }))
  })
  assert.equal(document.querySelector('.sched-conflict'), null, '還沒選 b2 不提示')
  await click(row('b2'))
  const hint = document.querySelector('.sched-conflict')
  assert.ok(hint, '同 Bot、時間相近要提示')
  assert.match(hint.textContent ?? '', /早上彙整/)
  assert.match(hint.textContent ?? '', /依序執行/)
  await click(row('b1'))
  assert.equal(document.querySelector('.sched-conflict'), null)
})

test('儲存列固定在底部（sticky），任務內容在表單最後', async () => {
  await openNew([])
  const actions = document.querySelector<HTMLElement>('.schedule-editor .form-actions')!
  assert.ok(actions, '儲存列存在')
  const prompt = document.querySelector<HTMLElement>('.schedule-editor .schedule-prompt')!
  assert.ok(prompt.compareDocumentPosition(actions) & Node.DOCUMENT_POSITION_FOLLOWING, '儲存列在任務內容之後')
  const css = await import('node:fs').then((fs) => fs.readFileSync(new URL('./scheduleEditor.css', import.meta.url), 'utf8'))
  assert.match(css, /\.schedule-editor \.form-actions \{[^}]*position: sticky/s)
})
