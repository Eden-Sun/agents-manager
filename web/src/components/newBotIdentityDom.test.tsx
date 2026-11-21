import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { BOT_KINDS, CREATABLE_KINDS } from '../api/types'
import { NewBotForm, Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})

let requests: FakeRequest[] = []

beforeEach(() => {
  requests = fakeApi((req) => (req.method === 'POST' && /\/projects\/p1\/bots$/.test(req.path) ? { id: 'nb1', name: 'codex-1' } : undefined))
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('b1', 'p1')],
    identities: [
      { name: 'work', kind: 'codex', env: { CODEX_HOME: '$HOME/.codex-work' }, args: [], host: null },
      { name: 'cc1', kind: 'claude', env: {}, args: [], host: null },
    ],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'b1', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

async function submitNewBot(kind: string, identity: string | null) {
  await mount(<Sidebar />)
  await settle()
  const open = [...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!
  await click(open)
  await settle()
  const pick = (group: string, label: string) =>
    [...document.querySelectorAll<HTMLButtonElement>(`[aria-label="${group}"] .opt`)].find((b) => b.textContent?.trim() === label)!
  await click(pick('kind', kind))
  if (identity !== null) await click(pick('身份', identity))
  await settle()
  const submit = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!
  await click(submit)
  await settle(50)
  const post = requests.find((r) => r.method === 'POST' && /\/projects\/p1\/bots$/.test(r.path))
  assert.ok(post, '要送出 POST /projects/p1/bots')
  return post.body as { kind: string; identity: string | null }
}

test('新增 Bot：codex 選了身份，送出的 body 帶那個 identity', async () => {
  const body = await submitNewBot('codex', 'work')
  assert.equal(body.kind, 'codex')
  assert.equal(body.identity, 'work')
})

test('claude 選身份照舊', async () => {
  const body = await submitNewBot('claude', 'cc1')
  assert.equal(body.kind, 'claude')
  assert.equal(body.identity, 'cc1')
})

test('不指定身份送 null', async () => {
  const body = await submitNewBot('codex', null)
  assert.equal(body.identity, null)
})

test('換 kind 會清掉身份：先選 claude 的 cc1 再切 codex 直接送出，送 null', async () => {
  await mount(<Sidebar />)
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!)
  await settle()
  const pick = (group: string, label: string) =>
    [...document.querySelectorAll<HTMLButtonElement>(`[aria-label="${group}"] .opt`)].find((b) => b.textContent?.trim() === label)!
  await click(pick('kind', 'claude'))
  await click(pick('身份', 'cc1'))
  await click(pick('kind', 'codex'))
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!)
  await settle(50)
  const post = requests.find((r) => r.method === 'POST' && /\/projects\/p1\/bots$/.test(r.path))
  assert.equal((post?.body as { identity: string | null } | undefined)?.identity, null)
})

test('kind 選項是 radio，選中的那顆 aria-checked=true', async () => {
  useStore.setState({ localTools: Object.fromEntries(BOT_KINDS.map((k) => [k, { installed: true, path: null, version: null, logged_in: true }])) } as never)
  await mount(<Sidebar />)
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!)
  await settle()
  const radios = () => [...document.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="kind"] [role="radio"]')]
  assert.equal(radios().length, CREATABLE_KINDS.length)
  const checked = () => radios().filter((b) => b.getAttribute('aria-checked') === 'true').map((b) => b.textContent?.trim())
  assert.deepEqual(checked(), ['claude'])
  await click(radios().find((b) => b.textContent?.trim() === 'codex')!)
  await settle()
  assert.deepEqual(checked(), ['codex'])
})

const LOGGED_OUT_HINT = '預設帳號還沒登入'
const toolsWithCodex = (logged_in: boolean | null) =>
  Object.fromEntries(BOT_KINDS.map((k) => [k, { installed: true, path: null, version: null, logged_in: k === 'codex' ? logged_in : true }]))
async function openNewBotWithKind(kind: string) {
  await mount(<Sidebar />)
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!)
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('[aria-label="kind"] .opt')].find((b) => b.querySelector('.opt-label')?.textContent?.trim() === kind)!)
  await settle()
}
const hasHint = () => (document.body.textContent ?? '').includes(LOGGED_OUT_HINT)

test('預設帳號沒登入、不指定身份：表單寫出警示，送出鈕仍可按', async () => {
  useStore.setState({ localTools: toolsWithCodex(false) } as never)
  await openNewBotWithKind('codex')
  assert.ok(hasHint(), '不指定身份而預設帳號沒登入，要寫出提示')
  const submit = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!
  assert.equal(submit.disabled, false, '只提示、不擋送出')
})

test('選了身份就不顯示預設帳號那一條（那邊有自己的警示）', async () => {
  useStore.setState({ localTools: toolsWithCodex(false) } as never)
  await openNewBotWithKind('codex')
  await click([...document.querySelectorAll<HTMLButtonElement>('[aria-label="身份"] .opt')].find((b) => b.textContent?.trim() === 'work')!)
  await settle()
  assert.equal(hasHint(), false)
})

test('預設帳號的登入狀態是 null（問不到）或 true：不顯示', async () => {
  for (const state of [null, true] as const) {
    await unmountAll()
    useStore.setState({ localTools: toolsWithCodex(state) } as never)
    await openNewBotWithKind('codex')
    assert.equal(hasHint(), false, `logged_in=${String(state)} 不下結論`)
  }
})

test('claude-api：不帶身份，未選模型時保留 daemon 預設，預設強度 high 由 daemon 補', async () => {
  useStore.setState({ localTools: toolsWithCodex(true) } as never)
  await openNewBotWithKind('claude-api')
  const text = document.querySelector('form.form')?.textContent ?? ''
  assert.ok(text.length > 0)
  assert.ok(!text.includes('身份'), 'claude-api 沒有帳號身份欄位')
  assert.ok(!document.querySelector('.share-profile-field'), '沒有分享設定')
  assert.ok(text.includes('Fast') && text.includes('單價 2 倍'), '有 fast 開關並標明單價 2 倍')
  await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!)
  await settle(50)
  const post = requests.find((r) => r.method === 'POST' && /\/projects\/p1\/bots$/.test(r.path))
  const body = post?.body as Record<string, unknown>
  assert.equal(body.kind, 'claude-api')
  assert.equal(body.identity, null)
  assert.equal(body.model, null)
  assert.equal(body.persona, null)
  assert.equal(body.fast, false)
})

test('claude-api 本機與遠端專案都能建：遠端專案那顆 kind 按鈕可按', async () => {
  useStore.setState({ projects: [{ ...project('p1'), host: 'mac_builder' }], hosts: [{ name: 'mac_builder', connected: true }], localTools: toolsWithCodex(true) } as never)
  await mount(<Sidebar />)
  await settle()
  await click([...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!)
  await settle()
  const btn = [...document.querySelectorAll<HTMLButtonElement>('[aria-label="kind"] .opt')].find((b) => b.querySelector('.opt-label')?.textContent?.trim() === 'claude-api')!
  assert.equal(btn.disabled, false)
})

test('claude-api：選 Fable 5.2 時 POST 保留完整模型 id', async () => {
  await openNewBotWithKind('claude-api')
  const option = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.includes('fable-5-2'))
  assert.ok(option, '模型選擇包含 Fable 5.2')
  await click(option)
  await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!)
  await settle(50)
  const post = requests.find((r) => r.method === 'POST' && /\/projects\/p1\/bots$/.test(r.path))
  assert.equal((post?.body as Record<string, unknown> | undefined)?.model, 'claude-fable-5-2')
})

test('切換專案時第一次 render 就反映新專案的預設 kind 與 name，不顯示舊專案資料', async () => {
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('b1', 'p1'), { ...bot('b2', 'p2'), kind: 'codex', name: 'codex-saved' }],
    localTools: Object.fromEntries(BOT_KINDS.map((k) => [k, { installed: true, path: null, version: null, logged_in: true }])),
  } as never)
  await mount(<NewBotForm onDone={() => {}} />)

  const kindOpt = () => document.querySelector<HTMLElement>('[aria-label="kind"] .opt.on .opt-label')?.textContent?.trim()
  const nameInput = () => document.querySelector<HTMLInputElement>('input[type="text"]:not(.opt-filter)')!
  assert.equal(kindOpt(), 'claude')

  const p2Btn = [...document.querySelectorAll<HTMLButtonElement>('[aria-label="Project"] .opt')].find((b) => b.textContent?.includes('proj-p2'))!
  assert.ok(p2Btn, '有 proj-p2 按鈕')
  await click(p2Btn)
  assert.equal(kindOpt(), 'codex', '第一次 render 即反映新專案預設 kind')
  assert.match(nameInput().value, /^codex/, '第一次 render 即反映新專案名稱')
})
