/**
 * 排程編輯器的「每次從基準快照開始」：選到的 Bot 有基準快照才能開；開了之後 PATCH 帶 `from_base`。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
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

const row = (over: Record<string, unknown> = {}) => ({
  id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日', prompt: '整理進度', mode: 'daily', tz: 'UTC',
  spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure',
  clear_after: false, from_base: false, created_at: '', updated_at: '', open: null, recent: [], ...over,
})

async function open(locked: boolean, schedule = row()) {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/bots/b1/base-snapshot')) return locked ? { locked: true, session_id: 'sid', locked_at: '2030-01-01T00:00:00.000Z' } : { locked: false }
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return schedule
    if (req.method === 'PATCH') return { ...schedule, ...(req.body as object) }
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const box = () => document.body.querySelector<HTMLInputElement>('.schedule-from-base input')!
  return { requests, box }
}

test('Bot 沒有基準快照：開關停用並說明要先鎖定', async () => {
  const { box } = await open(false)
  assert.equal(box().disabled, true)
  assert.match(document.body.textContent ?? '', /先到該 Bot 的設定按「鎖定目前 context 為基準」/)
})

test('Bot 有基準快照：可開，儲存時 PATCH 帶 from_base: true', async () => {
  const { requests, box } = await open(true)
  assert.equal(box().disabled, false)
  assert.equal(box().checked, false)
  await click(box())
  await click([...document.body.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
  await settle()
  const patch = requests.find((r) => r.method === 'PATCH')!
  assert.equal((patch.body as Record<string, unknown>).from_base, true)
})

test('已開著的排程，基準被清掉後仍可取消勾選', async () => {
  const { box } = await open(false, row({ from_base: true }))
  assert.equal(box().checked, true)
  assert.equal(box().disabled, false)
})
