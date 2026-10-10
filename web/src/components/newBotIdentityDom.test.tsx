/**
 * 側欄「新增 Bot」送出的身份（#1079）：codex／grok 選了身份也要帶出去，不能只有 claude 才送 identity。
 * 真的掛 `Sidebar` 進 happy-dom，點 kind 與身份、送出，看 POST 的 body。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { BOT_KINDS } from '../api/types'
import { Sidebar } from './Sidebar'

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

/** 開新增 Bot 表單，點 kind，（可選）點身份，送出；回傳送出的那筆 POST body。 */
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
  assert.equal(radios().length, BOT_KINDS.length)
  const checked = () => radios().filter((b) => b.getAttribute('aria-checked') === 'true').map((b) => b.textContent?.trim())
  assert.deepEqual(checked(), ['claude'])
  await click(radios().find((b) => b.textContent?.trim() === 'codex')!)
  await settle()
  assert.deepEqual(checked(), ['codex'])
})
