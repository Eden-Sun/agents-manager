/**
 * 專案排程的 DOM 測試（真的掛 `Sidebar` 進 happy-dom）：
 * - 專案「＋」展開成「新增 Bot／新增排程」，不再直接開 Bot 表單。
 * - 排程編輯器只列可接收排程的 Bot（同專案、一般 Bot；child 不列）。
 * - 建立時帶 client_request_id（冪等 key），後端回 409 時換新 key。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, keydown, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
import { useStore } from '../store/store'
import { Sidebar } from '../components/Sidebar'
import { ProjectSchedules } from './ProjectSchedules'

afterEach(unmountAll)
// 時區下拉約 445 個 <option>，在 happy-dom 裡每次開／重繪編輯器要 ~0.2 秒；這個檔案不測時區選項本身。
let tzList: ReturnType<typeof shortTimeZoneList>
before(async () => {
  await setupDom()
  tzList = shortTimeZoneList()
})
after(async () => {
  tzList.restore()
  await teardownDom()
})

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string, managedBy = 'user') => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: managedBy, parent_bot_id: managedBy === 'child' ? 'b1' : null,
  primary: false, primary_position: 0, cwd: null, share_profile: null,
})

beforeEach(() => {
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('b1', 'p1'), bot('b2', 'p1'), bot('b4', 'p1', 'child'), bot('b3', 'p2')],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'b2', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

const addButton = (label: string) => document.querySelector<HTMLButtonElement>(`[aria-label="在 ${label} 新增"]`)!
const menuItem = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent?.trim() === text)!
const dialogs = () => [...document.querySelectorAll<HTMLElement>('[role="dialog"]')]

test('專案「＋」展開成選單：新增 Bot／新增排程（不再直接開 Bot 表單）', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  assert.ok(menuItem('新增 Bot'), '選單要有「新增 Bot」')
  assert.ok(menuItem('新增排程'), '選單要有「新增排程」')
  assert.equal(dialogs().length, 0, '展開選單本身不能直接開任何對話框')
})

test('選單「新增排程」開編輯器，Bot 下拉只列同專案的一般 Bot（child 不列）', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  assert.ok(dialog, '要開出「新增排程」對話框')
  const options = [...dialog.querySelectorAll<HTMLOptionElement>('select option')].map((o) => o.value)
  assert.ok(options.includes('b1') && options.includes('b2'), '同專案的一般 Bot 要列出')
  assert.ok(!options.includes('b3'), '別的專案的 Bot 不列')
  assert.ok(!options.includes('b4'), 'child Bot 不列')
})

test('建立排程送出 client_request_id；後端 409（key 用在別的內容）時換新 key', async () => {
  const posts: string[] = []
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const method = init?.method ?? 'GET'
    if (method === 'GET' && String(input).includes('/schedules/s1')) {
      return new Response(JSON.stringify({ id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: '請整理今天的進度', tz: 'UTC', mode: 'daily', spec: { mode: 'daily', time: '09:00' }, next_due: null, enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null, recent: [] }), { status: 200 })
    }
    if (method === 'GET') return new Response(JSON.stringify({ schedules: [] }), { status: 200 })
    posts.push((JSON.parse(init?.body ?? '{}') as { client_request_id: string }).client_request_id)
    if (posts.length === 1) {
      return new Response(JSON.stringify({ error: 'conflict', reason: 'client_request_id_reused', message: '同一個 client_request_id 已用在不同內容的排程上' }), { status: 409 })
    }
    const body = { id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: '請整理今天的進度', tz: 'UTC', mode: 'daily', spec: { mode: 'daily', time: '09:00' }, next_due: null, enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }
    return new Response(JSON.stringify(body), { status: 200 })
  }) as typeof fetch
  try {
    await mount(<Sidebar />)
    await settle()
    await click(addButton('proj-p1'))
    await click(menuItem('新增排程'))
    await settle()
    const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
    await typeInto(dialog.querySelector<HTMLInputElement>('input')!, '每日摘要')
    await typeInto(dialog.querySelector<HTMLTextAreaElement>('textarea')!, '請整理今天的進度')
    const submit = () => [...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '建立排程')!
    await click(submit())
    await settle()
    await click(submit())
    await settle()
    assert.equal(posts.length, 2)
    assert.match(posts[0], /^[0-9a-f-]{36}$/, '第一次送出要帶 UUID 形式的 client_request_id')
    assert.notEqual(posts[0], posts[1], '409 之後要換新的 key')
  } finally {
    globalThis.fetch = realFetch
  }
})

test('專案 menu 列出排程（點開進編輯器）', async () => {
  fakeApi((req) => (req.method === 'GET' && req.path.includes('/projects/p1/schedules') ? { schedules: [{ id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: 'x', mode: 'daily', tz: 'UTC', spec: {}, next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }] } : undefined))
  await mount(<Sidebar />)
  await settle()
  const row = document.querySelector<HTMLButtonElement>('.project-schedule-row')
  assert.ok(row, '專案底下要有排程列')
  assert.match(row!.textContent ?? '', /每日摘要/)
  await act(async () => {})
})

test('排程 rev 變更後列表重抓；一般 schedules_changed 仍只更新指定專案', async () => {
  const schedule = (id: string, name: string, enabled = true) => ({
    id, project_id: 'p1', bot_id: 'b1', name, prompt: 'x', mode: 'daily', tz: 'UTC', spec: {},
    next_due: '2030-01-01T00:00:00.000Z', enabled, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null,
  })
  let rows = [schedule('s1', '斷線前')]
  const requests = fakeApi((req) => (req.method === 'GET' && req.path.includes('/projects/p1/schedules')
    ? { schedules: rows }
    : undefined))
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  const rowTexts = () => [...document.querySelectorAll('.project-schedule-row')].map((row) => row.textContent ?? '')
  const listGets = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/projects/p1/schedules')).length
  assert.match(rowTexts()[0], /斷線前/)
  assert.equal(listGets(), 1)

  rows = [schedule('s1', '斷線後改名', false), schedule('s2', '斷線期間新增')]
  await act(async () => useStore.setState((s) => ({ schedulesRev: s.schedulesRev + 1 })))
  await settle()
  assert.equal(rowTexts().length, 2, '掛載中的列表要補上斷線期間新增的排程')
  assert.match(rowTexts()[0], /斷線後改名/, '掛載中的列表要更新漏掉的改名')
  assert.match(rowTexts()[0], /已停用/, '掛載中的列表要更新漏掉的停用狀態')
  assert.match(rowTexts()[1], /斷線期間新增/)
  assert.equal(listGets(), 2)

  await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p2', id: 's1' } })))
  await settle()
  assert.equal(listGets(), 2, '其他專案的正常事件不能觸發重抓')
  await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p1', id: 's1' } })))
  await settle()
  assert.equal(listGets(), 3, '同專案的正常事件仍要觸發重抓')
})

test('列表新 GET 先回時，較舊的排程回應不能覆蓋新資料', async () => {
  const schedule = (name: string) => ({
    id: 's1', project_id: 'p1', bot_id: 'b1', name, prompt: 'x', mode: 'daily', tz: 'UTC', spec: {},
    next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null,
  })
  const realFetch = globalThis.fetch
  let listGets = 0
  let releaseOld: ((response: Response) => void) | null = null
  const response = (name: string) => new Response(JSON.stringify({ schedules: [schedule(name)] }), { status: 200 })
  globalThis.fetch = (async () => {
    listGets += 1
    if (listGets === 1) return response('初始')
    if (listGets === 2) return await new Promise<Response>((resolve) => { releaseOld = resolve })
    return response('重連後最新')
  }) as unknown as typeof fetch
  try {
    await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
    await settle()
    assert.equal(listGets, 1)

    await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p1', id: 's1' } })))
    assert.equal(listGets, 2, '先發出較舊的事件重抓，並扣住回應')
    await act(async () => useStore.setState((s) => ({ schedulesRev: s.schedulesRev + 1 })))
    await settle()
    const rowText = () => document.querySelector('.project-schedule-row')?.textContent ?? ''
    assert.equal(listGets, 3)
    assert.match(rowText(), /重連後最新/)

    assert.ok(releaseOld)
    await act(async () => releaseOld!(response('較舊事件回應')))
    await settle()
    assert.match(rowText(), /重連後最新/)
    assert.doesNotMatch(rowText(), /較舊事件回應/)
  } finally {
    globalThis.fetch = realFetch
  }
})

test('綁定 turn 的狀態：queued→in_flight→終態，列表即時跟上（不靠 schedules_changed）', async () => {
  // 後端 tick 只在終態才推 schedules_changed；occurrence 在 in_flight 期間仍是 queued，所以列要看自己綁的 turn。
  let backend: { open: Record<string, unknown> | null; next: string | null } = {
    open: { id: 'o1', schedule_id: 's1', source: 'scheduled', scheduled_for: '', generation: 1, bot_id: 'b1', prompt: 'x', status: 'queued', waiting_reason: null, turn_id: 't1', turn_status: 'queued', turn_delivery: 'pending', merged_count: 0, error: null, created_at: '', updated_at: '' },
    next: null,
  }
  let listGets = 0
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.includes('/projects/p1/schedules')) {
      listGets += 1
      return { schedules: [{ id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: 'x', mode: 'daily', tz: 'UTC', spec: {}, next_due: backend.next ?? '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: backend.open }] }
    }
    return undefined
  })
  useStore.setState({ turns: { b1: { t1: { id: 't1', status: 'queued', delivery: 'pending' } } } } as never)
  await mount(<Sidebar />)
  await settle()
  const rowText = () => document.querySelector('.project-schedule-row')?.textContent ?? ''
  assert.match(rowText(), /已排隊，等待 Bot 接收/, '排隊中要顯示排隊')
  const before = listGets

  await act(async () => useStore.setState({ turns: { b1: { t1: { id: 't1', status: 'in_flight', delivery: 'ok' } } } } as never))
  await settle()
  assert.match(rowText(), /執行中/, 'turn 開始送出後要改成執行中，不能一直顯示排隊')
  assert.ok(listGets > before, '綁定 turn 變了才重抓這一列')

  // 終態：後端 settle 之後 open 清空；前端 turn 一變就要抓到這個結果。
  backend = { open: null, next: '2030-01-02T00:00:00.000Z' }
  await act(async () => useStore.setState({ turns: { b1: { t1: { id: 't1', status: 'completed', delivery: 'ok' } } } } as never))
  await settle()
  assert.doesNotMatch(rowText(), /執行中|已排隊/, '終態後不再顯示執行中')
  assert.match(rowText(), /下次/, '回到下一次的時間')
})

test('別的 Bot 的 turn 更新不會讓這一列重抓（只看自己綁的 turn）', async () => {
  let listGets = 0
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.includes('/projects/p1/schedules')) {
      listGets += 1
      return { schedules: [{ id: 's1', project_id: 'p1', bot_id: 'b1', name: 'n', prompt: 'x', mode: 'daily', tz: 'UTC', spec: {}, next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: { id: 'o1', schedule_id: 's1', source: 'scheduled', scheduled_for: '', generation: 1, bot_id: 'b1', prompt: 'x', status: 'queued', waiting_reason: null, turn_id: 't1', turn_status: 'queued', turn_delivery: 'pending', merged_count: 0, error: null, created_at: '', updated_at: '' } }] }
    }
    return undefined
  })
  useStore.setState({ turns: { b1: { t1: { id: 't1', status: 'queued', delivery: 'pending' } } } } as never)
  await mount(<Sidebar />)
  await settle()
  const before = listGets
  await act(async () => useStore.setState({ turns: { b1: { t1: { id: 't1', status: 'queued', delivery: 'pending' }, t9: { id: 't9', status: 'in_flight', delivery: 'ok' } } } } as never))
  await settle()
  assert.equal(listGets, before, '無關的 turn 變化不觸發重抓')
})

test('非 secure（LAN http）沒有 crypto.randomUUID：整個側欄開排程編輯器不能炸（白屏）', async () => {
  const realCrypto = globalThis.crypto
  // LAN IP 開網頁時 randomUUID 不存在；getRandomValues 仍在。
  Object.defineProperty(globalThis, 'crypto', {
    configurable: true,
    value: { getRandomValues: (a: never) => realCrypto.getRandomValues(a) },
  })
  try {
    fakeApi(() => ({ schedules: [] }))
    await mount(<Sidebar />)
    await settle()
    await click(addButton('proj-p1'))
    await click(menuItem('新增排程'))
    await settle()
    const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))
    assert.ok(dialog, '編輯器要真的打開')
    assert.ok(dialog!.querySelector('input'), '表單要渲染出來')
  } finally {
    Object.defineProperty(globalThis, 'crypto', { configurable: true, value: realCrypto })
  }
})

test('完整流程（整個 Sidebar）：＋→新增排程→選 am-codex→建立→開詳情→修改→儲存，全程不白屏', async () => {
  // 有狀態的假後端，回應形狀照 daemon：建立／儲存回 Schedule（不帶 recent），詳情才帶 recent。
  const rows: Record<string, unknown>[] = []
  fakeApi((req) => {
    const p = req.path
    if (req.method === 'GET' && p.endsWith('/projects/p1/schedules')) return { schedules: rows.map((r) => ({ ...r })) }
    if (req.method === 'POST' && p.endsWith('/projects/p1/schedules')) {
      const b = req.body as Record<string, unknown>
      const row = {
        id: 's1', project_id: 'p1', bot_id: b.bot_id, name: b.name, prompt: b.prompt, mode: 'daily', tz: b.tz, spec: b.spec,
        next_due: '2030-01-01T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null,
      }
      rows.push(row)
      return { ...row }
    }
    const id = p.match(/\/schedules\/([^/]+)$/)?.[1]
    if (id && req.method === 'GET') return { ...rows.find((r) => r.id === id), recent: [] }
    if (id && req.method === 'PATCH') {
      const r = rows.find((x) => x.id === id)!
      Object.assign(r, req.body as Record<string, unknown>)
      return { ...r }
    }
    return undefined
  })
  useStore.setState({ bots: [...(useStore.getState().bots as never[]), bot('b5', 'p1')].map((b) => (b as { id: string }).id === 'b5' ? { ...(b as object), name: 'am-codex', kind: 'codex' } : b) } as never)
  await mount(<Sidebar />)
  await settle()

  // 1) 新增排程，選 am-codex，填名稱與內容，建立。
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  let dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  await typeInto(dialog.querySelector<HTMLInputElement>('input')!, '每日摘要')
  await typeInto(dialog.querySelector<HTMLTextAreaElement>('textarea')!, '請整理今天的進度')
  const botSelect = dialog.querySelectorAll<HTMLSelectElement>('select')[0]
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(botSelect, 'b5')
    botSelect.dispatchEvent(new Event('change', { bubbles: true }))
  })
  assert.equal(botSelect.value, 'b5', '要真的選到 am-codex')
  await click([...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '建立排程')!)
  await settle()

  // 2) 建立後編輯器轉成編輯模式，詳情載得進來，整頁還在。
  assert.equal(rows.length, 1, '後端要收到一筆')
  assert.equal((rows[0].bot_id as string), 'b5')
  assert.ok(document.querySelector('.sidebar, nav, body'), '整頁不能空白')
  dialog = dialogs().find((d) => d.textContent?.includes('排程') && d.querySelector('input'))!
  assert.ok(dialog, '建立後排程 modal 還要在')
  assert.match(dialog.textContent ?? '', /還沒有執行紀錄|狀態：/, '詳情區要渲染（不能因為缺 recent 炸掉）')

  // 3) 修改名稱並儲存。
  await typeInto(dialog.querySelector<HTMLInputElement>('input')!, '每日摘要 v2')
  await click([...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
  await settle()
  assert.equal(rows[0].name, '每日摘要 v2', '後端要收到修改')
  dialog = dialogs().find((d) => d.textContent?.includes('排程') && d.querySelector('input'))!
  assert.ok(dialog, '儲存後 modal 還要在、不能白屏')
  assert.equal(dialog.querySelector<HTMLInputElement>('input')!.value, '每日摘要 v2')
})

test('編輯器掛在 document.body，不在側欄容器裡（手機上側欄 transform 會把 fixed 遮罩限寬）', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  assert.ok(dialog, '編輯器要打開')
  assert.equal(dialog.closest('nav.sidebar-scroll'), null, '彈窗不能在側欄 nav 裡')
  assert.ok(document.body.contains(dialog), '彈窗要在 body 底下')
})

test('編輯器有「開啟 Bot 對話」：切到該 Bot 並關掉編輯器', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  const open = [...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '開啟 Bot 對話')!
  assert.ok(open, '要有開啟 Bot 對話的按鈕')
  await click(open)
  await settle()
  assert.equal(useStore.getState().selectedBotId, 'b1', '切到編輯器選的 Bot')
  assert.equal(dialogs().filter((d) => d.textContent?.includes('新增排程')).length, 0, '編輯器要關掉')
})

test('手動執行：POST 已送達但回應遺失 → 重按用同一個 key，只產生一次執行（不重複 Model job）', async () => {
  const realFetch = globalThis.fetch
  const seenKeys = new Set<string>()
  const runKeysSent: string[] = []
  let jobs = 0
  let loseNextResponse = true
  const schedule = { id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: 'x', mode: 'daily', tz: 'UTC', spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const path = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'POST' && path.endsWith('/schedules/s1/run')) {
      const key = (JSON.parse(init?.body ?? '{}') as { client_request_id: string }).client_request_id
      runKeysSent.push(key)
      // 伺服器：同一個 key 只跑一次（回原來那一筆）。
      if (!seenKeys.has(key)) {
        seenKeys.add(key)
        jobs += 1
      }
      if (loseNextResponse) {
        loseNextResponse = false
        throw new TypeError('network error: response lost')
      }
      return new Response(JSON.stringify({ occurrence: { id: 'occ1', schedule_id: 's1', source: 'manual', status: 'queued', turn_status: 'queued', scheduled_for: '', generation: 1, bot_id: 'b1', prompt: 'x', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null, error: null, created_at: '', updated_at: '', turn_delivery: null } }), { status: 200 })
    }
    if (method === 'GET' && path.endsWith('/projects/p1/schedules')) return new Response(JSON.stringify({ schedules: [schedule] }), { status: 200 })
    if (method === 'GET' && path.endsWith('/schedules/s1')) return new Response(JSON.stringify({ ...schedule, recent: [] }), { status: 200 })
    return new Response('{}', { status: 200 })
  }) as typeof fetch
  try {
    await mount(<Sidebar />)
    await settle()
    await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
    await settle()
    const runBtn = () => [...dialogs().find((d) => d.textContent?.includes('排程'))!.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '立即執行')!
    await click(runBtn())
    await settle()
    await click(runBtn())
    await settle()
    assert.equal(runKeysSent.length, 2, '兩次送出都到了伺服器')
    assert.equal(runKeysSent[0], runKeysSent[1], '重按要用同一個 key')
    assert.equal(jobs, 1, '同一個 key 只產生一次執行')
    // 收到回應後，下一次按才換新的 key。
    await click(runBtn())
    await settle()
    assert.notEqual(runKeysSent[2], runKeysSent[0], '成功收下回應後，下一次明確執行用新 key')
  } finally {
    globalThis.fetch = realFetch
  }
})

test('手動執行 key 存在共用 createRequestId：關掉編輯器再開（或重整）仍沿用同一個 key', async () => {
  const realFetch = globalThis.fetch
  const runKeysSent: string[] = []
  const schedule = { id: 's2', project_id: 'p1', bot_id: 'b1', name: '重開測試', prompt: 'x', mode: 'daily', tz: 'UTC', spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }
  let loseNext = true
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const path = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'POST' && path.endsWith('/schedules/s2/run')) {
      runKeysSent.push((JSON.parse(init?.body ?? '{}') as { client_request_id: string }).client_request_id)
      if (loseNext) {
        loseNext = false
        throw new TypeError('network error')
      }
      return new Response(JSON.stringify({ occurrence: null }), { status: 200 })
    }
    if (method === 'GET' && path.endsWith('/projects/p1/schedules')) return new Response(JSON.stringify({ schedules: [schedule] }), { status: 200 })
    if (method === 'GET' && path.endsWith('/schedules/s2')) return new Response(JSON.stringify({ ...schedule, recent: [] }), { status: 200 })
    return new Response('{}', { status: 200 })
  }) as typeof fetch
  try {
    await mount(<Sidebar />)
    await settle()
    const runIn = () => [...dialogs().find((d) => d.textContent?.includes('排程'))!.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '立即執行')!
    await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
    await settle()
    await click(runIn())
    await settle()
    // 關掉編輯器（Escape），再從列表開回來（同一個分頁；sessionStorage 留著 key）。
    await act(async () => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    })
    await settle()
    assert.equal(dialogs().filter((d) => d.textContent?.includes('排程')).length, 0, '編輯器要關掉')
    await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
    await settle()
    await click(runIn())
    await settle()
    assert.equal(runKeysSent.length, 2)
    assert.equal(runKeysSent[0], runKeysSent[1], '關掉再開，手動執行仍用同一個 key')
  } finally {
    globalThis.fetch = realFetch
  }
})

test('手動執行：POST 已收下、詳情 GET 503 → 再按用同一個 key，回同一筆 occurrence，不重跑', async () => {
  const realFetch = globalThis.fetch
  const keys: string[] = []
  const occIdsReturned: string[] = []
  const processed = new Set<string>()
  let jobs = 0
  let detailCalls = 0
  const schedule = { id: 's3', project_id: 'p1', bot_id: 'b1', name: '503測試', prompt: 'x', mode: 'daily', tz: 'UTC', spec: { mode: 'daily', time: '09:00' }, next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const path = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'POST' && path.endsWith('/schedules/s3/run')) {
      const key = (JSON.parse(init?.body ?? '{}') as { client_request_id: string }).client_request_id
      keys.push(key)
      // 伺服器：同一個 key 只產生一筆、之後都回同一筆。
      if (!processed.has(key)) {
        processed.add(key)
        jobs += 1
      }
      occIdsReturned.push('occ-503')
      return new Response(JSON.stringify({ occurrence: { id: 'occ-503', schedule_id: 's3', source: 'manual', status: 'queued', turn_status: 'queued', scheduled_for: '', generation: 1, bot_id: 'b1', prompt: 'x', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null, error: null, created_at: '', updated_at: '', turn_delivery: null } }), { status: 200 })
    }
    if (method === 'GET' && path.endsWith('/projects/p1/schedules')) return new Response(JSON.stringify({ schedules: [schedule] }), { status: 200 })
    if (method === 'GET' && path.endsWith('/schedules/s3')) {
      detailCalls += 1
      // 第 1 次是開編輯器載入詳情；第 2 次是 POST 收下之後的重抓 → 503；之後正常。
      if (detailCalls === 2) return new Response('{"error":"unavailable"}', { status: 503 })
      return new Response(JSON.stringify({ ...schedule, recent: [] }), { status: 200 })
    }
    return new Response('{}', { status: 200 })
  }) as typeof fetch
  try {
    await mount(<Sidebar />)
    await settle()
    await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
    await settle()
    const runBtn = () => [...dialogs().find((d) => d.textContent?.includes('排程'))!.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '立即執行')!
    await click(runBtn())
    await settle()
    assert.equal(keys.length, 1)
    assert.match(dialogs().find((d) => d.textContent?.includes('排程'))!.textContent ?? '', /503|unavailable|錯誤|error/i, '詳情 503 要讓使用者看到錯誤')
    await click(runBtn())
    await settle()
    assert.equal(keys.length, 2, '再按一次要真的送出')
    assert.equal(keys[0], keys[1], '詳情 503 後再按，沿用同一個 key')
    assert.deepEqual(occIdsReturned, ['occ-503', 'occ-503'], '回同一筆 occurrence')
    assert.equal(jobs, 1, '只產生一次執行')
    // 這次成功收下（含詳情）之後，下一次明確執行換新的 key。
    await click(runBtn())
    await settle()
    assert.notEqual(keys[2], keys[0], '成功之後下一次用新 key')
  } finally {
    globalThis.fetch = realFetch
  }
})

const scheduleRow = (over: Record<string, unknown> = {}) => ({
  id: 's1', project_id: 'p1', bot_id: 'b1', name: '每日摘要', prompt: '請整理今天的進度', mode: 'daily', tz: 'UTC', spec: { mode: 'daily', time: '09:00' },
  next_due: '2030-01-01T00:00:00.000Z', enabled: true, generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', open: null, ...over,
})

test('編輯器：任務內容在名稱之後、Bot 選單之前，textarea 至少 8 行', async () => {
  fakeApi(() => ({ schedules: [] }))
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  const name = dialog.querySelector<HTMLInputElement>('input')!
  const prompt = dialog.querySelector<HTMLTextAreaElement>('textarea')!
  const bot = dialog.querySelector<HTMLSelectElement>('select')!
  const follows = Node.DOCUMENT_POSITION_FOLLOWING
  assert.ok(name.compareDocumentPosition(prompt) & follows, '任務內容要在名稱之後')
  assert.ok(prompt.compareDocumentPosition(bot) & follows, '任務內容要在 Bot 選單之前')
  assert.equal(prompt.getAttribute('rows'), '8', '任務內容框最少 8 行')
  assert.match(dialog.textContent ?? '', /⌘\/Ctrl\+Enter 儲存/, 'label 旁要有快捷鍵提示')
})

test('編輯器：最近執行收在預設收合的 <details> 裡，放在表單最下方', async () => {
  const recent = [{ id: 'o1', schedule_id: 's1', source: 'scheduled', scheduled_for: '2030-01-01T00:00:00.000Z', generation: 1, bot_id: 'b1', prompt: 'x', status: 'done', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null, error: null, created_at: '', updated_at: '' }]
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [scheduleRow()] }
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return { ...scheduleRow(), recent }
    return undefined
  })
  await mount(<Sidebar />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('排程'))!
  const details = dialog.querySelector<HTMLDetailsElement>('details.schedule-recent')!
  assert.ok(details, '最近執行要在 details 裡')
  assert.equal(details.open, false, '預設收合')
  assert.match(details.querySelector('summary')!.textContent ?? '', /最近執行（1）/)
  assert.ok(details.querySelector('ul[aria-label="最近執行"]'), '列表還在 details 內')
  assert.equal(dialog.querySelector('.schedule-status ul[aria-label="最近執行"]'), null, '頂部不再放列表')
  const form = dialog.querySelector('.schedule-editor')!
  assert.equal(form.lastElementChild, details, 'details 是表單最後一個區塊')
})

test('編輯器：Ctrl+Enter（⌘+Enter）＝儲存；單按 Enter 只是換行', async () => {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [scheduleRow()] }
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return { ...scheduleRow(), recent: [] }
    if (req.method === 'PATCH') return scheduleRow({ prompt: (req.body as { prompt: string }).prompt })
    return undefined
  })
  await mount(<Sidebar />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('排程'))!
  const prompt = dialog.querySelector<HTMLTextAreaElement>('textarea')!
  await typeInto(prompt, '第一行\n第二行')
  await keydown(prompt, 'Enter')
  await settle()
  assert.equal(requests.filter((r) => r.method === 'PATCH').length, 0, '單按 Enter 不儲存')
  await keydown(prompt, 'Enter', { ctrlKey: true })
  await settle()
  await keydown(prompt, 'Enter', { metaKey: true })
  await settle()
  const patches = requests.filter((r) => r.method === 'PATCH')
  assert.ok(patches.length >= 1, 'Ctrl+Enter 要送出儲存')
  assert.equal((patches[0].body as { prompt: string }).prompt, '第一行\n第二行', '送的是任務內容')
})

test('編輯器：Ctrl+Enter 在輸入法選字中不儲存', async () => {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [scheduleRow()] }
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return { ...scheduleRow(), recent: [] }
    return undefined
  })
  await mount(<Sidebar />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('排程'))!
  const prompt = dialog.querySelector<HTMLTextAreaElement>('textarea')!
  // 選字中的 Ctrl+Enter：isComposing 為真，不能當成儲存。
  const composing = new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true, cancelable: true })
  Object.defineProperty(composing, 'isComposing', { value: true })
  await act(async () => {
    prompt.dispatchEvent(composing)
  })
  await settle()
  assert.equal(requests.filter((r) => r.method === 'PATCH').length, 0)
})

test('「跑完就清空對話」勾選：建立送出 clear_after:true；編輯時可改、GET 回的值會勾上', async () => {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [] }
    if (req.method === 'POST') return scheduleRow({ clear_after: true })
    return undefined
  })
  await mount(<Sidebar />)
  await settle()
  await click(addButton('proj-p1'))
  await click(menuItem('新增排程'))
  await settle()
  const dialog = dialogs().find((d) => d.textContent?.includes('新增排程'))!
  assert.match(dialog.textContent ?? '', /建議搭配專用 Bot/, '勾選框下要有說明')
  await typeInto(dialog.querySelector<HTMLInputElement>('input')!, '每日摘要')
  await typeInto(dialog.querySelector<HTMLTextAreaElement>('textarea')!, '請整理今天的進度')
  const check = dialog.querySelector<HTMLInputElement>('input[type="checkbox"]')!
  assert.equal(check.checked, false, '預設不勾')
  await act(async () => check.click())
  await click([...dialog.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '建立排程')!)
  await settle()
  const post = requests.find((r) => r.method === 'POST')!
  assert.equal((post.body as { clear_after: boolean }).clear_after, true, '建立要送 clear_after:true')

  // 編輯既有排程：GET 回 true → 勾選框是勾的；取消勾選後 PATCH 送 false。
  await unmountAll()
  const editRequests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/projects/p1/schedules')) return { schedules: [scheduleRow({ clear_after: true })] }
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return { ...scheduleRow({ clear_after: true }), recent: [] }
    if (req.method === 'PATCH') return scheduleRow({ clear_after: false })
    return undefined
  })
  await mount(<Sidebar />)
  await settle()
  await click(document.querySelector<HTMLButtonElement>('.project-schedule-row')!)
  await settle()
  const edit = dialogs().find((d) => d.textContent?.includes('排程'))!
  const editCheck = edit.querySelector<HTMLInputElement>('input[type="checkbox"]')!
  assert.equal(editCheck.checked, true, '編輯時要反映 clear_after')
  await act(async () => editCheck.click())
  await click([...edit.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
  await settle()
  const patch = editRequests.find((r) => r.method === 'PATCH')!
  assert.equal((patch.body as { clear_after: boolean }).clear_after, false)
})
