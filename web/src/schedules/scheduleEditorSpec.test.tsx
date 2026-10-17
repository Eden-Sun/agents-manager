/**
 * #1807：編輯排程時只有排程時間真的變了才送 `spec`。沒動時間就不帶 spec（後端會把 spec 當成新的
 * 間隔錨點重算、升 generation），而且秒精度要保留（API 收的是秒，61 秒不能被四捨五入成 60 秒）。
 * 用正式的 `ScheduleEditor`，回應由假後端扮演。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ScheduleEditor } from './ScheduleEditor'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const scheduleRow = (over: Record<string, unknown> = {}) => ({
  id: 's1', project_id: 'p1', bot_id: 'b1', name: '每分鐘摘要', prompt: '整理進度', mode: 'interval', tz: 'UTC',
  spec: { mode: 'interval', every_secs: 61, anchor: '2030-01-01T00:00:00.000Z' }, next_due: '2030-01-01T00:01:00.000Z',
  enabled: true, generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', open: null, recent: [],
  ...over,
})

/** 掛上編輯器，等詳情載入完，回傳對話框與假後端紀錄的 PATCH 送出內容。 */
async function openEditor(row: Record<string, unknown>) {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return row
    if (req.method === 'PATCH') return { ...row, ...(req.body as object) }
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  // 編輯器掛在 document.body（portal），整個頁面找就好。
  const root = document.body
  const patches = () => requests.filter((r) => r.method === 'PATCH').map((r) => r.body as Record<string, unknown>)
  const save = async () => {
    await click([...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
    await settle()
  }
  return { root, patches, save }
}

test('61 秒的間隔，沒動時間只按儲存：PATCH 不帶 spec（秒精度與錨點都不動）', async () => {
  const { patches, save } = await openEditor(scheduleRow())
  await save()
  assert.equal(patches().length, 1)
  assert.equal('spec' in patches()[0], false, '沒改時間就不能送 spec')
})

test('整分鐘的間隔，沒動時間只按儲存：PATCH 也不帶 spec（no-op 不重設週期）', async () => {
  const { patches, save } = await openEditor(scheduleRow({ spec: { mode: 'interval', every_secs: 120, anchor: '2030-01-01T00:00:00.000Z' } }))
  await save()
  assert.equal('spec' in patches()[0], false)
})

test('只改 clear_after：PATCH 只有非 spec 欄位，spec 不送', async () => {
  const { root, patches, save } = await openEditor(scheduleRow())
  const checkbox = root.querySelector<HTMLInputElement>('.schedule-clear-after input')!
  await click(checkbox)
  await save()
  assert.equal('spec' in patches()[0], false, '只改 clear_after 不能帶 spec')
  assert.equal(patches()[0].clear_after, true)
})

test('真的改了分鐘：送新的 spec（秒數依輸入）', async () => {
  const { root, patches, save } = await openEditor(scheduleRow())
  const minutes = root.querySelector<HTMLInputElement>('input[type="number"]')!
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(minutes, '2')
    minutes.dispatchEvent(new Event('input', { bubbles: true }))
    minutes.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await save()
  assert.deepEqual(patches()[0].spec, { mode: 'interval', every_secs: 120 })
})
