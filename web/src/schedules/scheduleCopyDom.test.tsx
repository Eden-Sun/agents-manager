import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import type { ScheduleDetail, ScheduleSpecIn } from '../api/schedules'
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
    bots: [{ id: 'b1', name: '原本的 Bot', project_id: 'p1', kind: 'claude', model: null, effort: null, managed_by: 'user', parent_bot_id: null, share_profile: null }],
    runs: {}, notices: [], schedulesRev: 0,
  } as never)
})

function source(spec: ScheduleSpecIn): ScheduleDetail {
  return {
    id: 'source', project_id: 'p1', bot_id: 'b1', name: '原排程', prompt: '既有任務內容',
    mode: spec.mode, spec, tz: 'Asia/Taipei', notify: 'all', clear_after: true,
    enabled: false, generation: 8, next_due: '2030-01-01T00:00:00Z', created_at: '', updated_at: '',
    open: null, recent: [],
  }
}

const menuItem = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent?.trim() === text)!
const dialog = () => document.querySelector<HTMLElement>('[role="dialog"]')!
const submit = () => [...dialog().querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '建立排程')!

async function openMenu() {
  await click(document.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!)
}

test('排程操作選單沿用編輯入口，開選單不建立副本', async () => {
  const s = source({ mode: 'daily', time: '09:00' })
  const requests = fakeApi(() => ({ schedules: [s] }))
  const selected: string[] = []
  await mount(<ProjectSchedules projectId="p1" onOpen={(id) => selected.push(id)} />)
  await settle()
  await openMenu()
  assert.ok(menuItem('複製排程…'))
  await click(menuItem('編輯排程…'))
  assert.deepEqual(selected, ['source'])
  assert.equal(document.querySelector('[role="menu"]'), null)
  assert.equal(requests.filter((r) => r.method !== 'GET').length, 0)
})

const modes: ScheduleSpecIn[] = [
  { mode: 'daily', time: '23:17' },
  { mode: 'weekly', time: '05:31', weekdays: [1, 4, 6] },
  { mode: 'weekly', time: '08:26', weekdays: [4, 2, 0, 3, 1] },
  { mode: 'interval', every_secs: 61 },
  { mode: 'once', at: '2035-07-08T14:25' },
]

for (const spec of modes) {
  test(`複製 ${spec.mode}：取最新設定、先編輯再建立，完整保留 spec 與原 Bot`, async () => {
    const original = source(spec)
    const latest = { ...original, name: '最新名稱', prompt: '最新任務正文', spec: { ...spec, anchor: '2000-01-01T00:00:00Z' } }
    const requests = fakeApi((r) => {
      if (r.path.endsWith('/projects/p1/schedules') && r.method === 'GET') return { schedules: [original] }
      if (r.path.endsWith('/schedules/source')) return latest
      if (r.method === 'POST') return { ...original, ...r.body as object, id: 'copy', enabled: true, generation: 1 }
    })
    const selected: string[] = []
    await mount(<ProjectSchedules projectId="p1" onOpen={(id) => selected.push(id)} />)
    await settle()
    await openMenu()
    await click(menuItem('複製排程…'))
    await settle()
    assert.match(dialog().textContent ?? '', /複製排程/)
    assert.equal(dialog().querySelector<HTMLInputElement>('input')!.value, '最新名稱（副本）')
    assert.equal(dialog().querySelector<HTMLTextAreaElement>('textarea')!.value, '最新任務正文')
    assert.equal(requests.filter((r) => r.method !== 'GET').length, 0, '按複製只預填，不能先寫入或執行')
    assert.doesNotMatch(dialog().textContent ?? '', /最近執行|立即執行/)
    if (spec.mode === 'weekly' && spec.weekdays?.length === 5) {
      const mode = [...dialog().querySelectorAll<HTMLSelectElement>('select')].find((el) => el.querySelector('option[value=weekdays]'))!
      assert.equal(mode.value, 'weekdays')
      assert.equal(mode.selectedOptions[0].textContent, '週一至週五')
      assert.equal(dialog().querySelector('fieldset'), null)
    }
    await typeInto(dialog().querySelector<HTMLInputElement>('input')!, '自行改的副本名')
    await click(submit())
    await settle()
    const posts = requests.filter((r) => r.method === 'POST')
    assert.equal(posts.length, 1)
    assert.ok(posts[0].path.endsWith('/projects/p1/schedules'))
    const { client_request_id, ...body } = posts[0].body as Record<string, unknown>
    assert.match(String(client_request_id), /^[0-9a-f-]{36}$/)
    assert.deepEqual(body, {
      name: '自行改的副本名', prompt: '最新任務正文', bot_id: 'b1', tz: 'Asia/Taipei',
      spec: spec.mode === 'weekly' ? { ...spec, weekdays: [...spec.weekdays!].sort((a, b) => a - b) } : spec, notify: 'all', clear_after: true,
    }, '建立模式必須送出完整 spec，不能因 baseline 判成未改而省略；interval 另起新 anchor')
    assert.deepEqual(selected, ['copy'])
    assert.equal(document.querySelector('[role="dialog"]'), null)
    assert.equal(original.name, '原排程')
    assert.equal(original.enabled, false)
    assert.equal(requests.filter((r) => r.method === 'PATCH' || r.path.includes('/run') || r.path.endsWith('/bots')).length, 0)
  })
}

test('取消複製不建立排程；長 Unicode 名稱保留完整字元與副本後綴', async () => {
  const s = { ...source({ mode: 'daily', time: '09:00' }), name: '😀'.repeat(39) + '好' }
  const requests = fakeApi((r) => r.path.endsWith('/schedules/source') ? s : { schedules: [s] })
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  await openMenu()
  await click(menuItem('複製排程…'))
  await settle()
  const name = dialog().querySelector<HTMLInputElement>('input')!.value
  assert.equal(name, '😀'.repeat(38) + '（副本）')
  assert.equal(name.length, 80)
  await click(dialog().querySelector<HTMLButtonElement>('[aria-label="關閉"]')!)
  assert.equal(document.querySelector('[role="dialog"]'), null)
  assert.equal(requests.filter((r) => r.method !== 'GET').length, 0)
})

test('讀取複製來源失敗要提示，不開空白表單或送建立請求', async () => {
  const s = source({ mode: 'daily', time: '09:00' })
  const requests = fakeApi((r) => {
    if (r.path.endsWith('/schedules/source')) throw new Error('來源讀取失敗')
    return { schedules: [s] }
  })
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  await openMenu()
  await click(menuItem('複製排程…'))
  await settle()
  assert.equal(document.querySelector('[role="dialog"]'), null)
  assert.equal(requests.filter((r) => r.method !== 'GET').length, 0)
  assert.ok(useStore.getState().notices.some((n) => n.kind === 'error' && n.text.includes('複製排程失敗')))
})

test('副本建立回應 503 後重送沿用同一 client_request_id', async () => {
  const s = source({ mode: 'interval', every_secs: 61 })
  const realFetch = globalThis.fetch
  const keys: string[] = []
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    if (init?.method === 'POST') {
      keys.push(JSON.parse(init.body!).client_request_id)
      return new Response(JSON.stringify(keys.length === 1 ? { error: '暫時無法回應' } : { ...s, id: 'copy' }), { status: keys.length === 1 ? 503 : 200 })
    }
    return new Response(JSON.stringify(String(input).endsWith('/schedules/source') ? s : { schedules: [s] }), { status: 200 })
  }) as typeof fetch
  try {
    await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
    await settle()
    await openMenu()
    await click(menuItem('複製排程…'))
    await settle()
    await click(submit())
    await settle()
    assert.ok(dialog().querySelector('[role="alert"]'))
    await click(submit())
    await settle()
    assert.equal(keys.length, 2)
    assert.equal(keys[0], keys[1])
  } finally {
    globalThis.fetch = realFetch
  }
})
