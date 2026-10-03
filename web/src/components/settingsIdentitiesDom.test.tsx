/**
 * 環境設定的「身份」面板（IdentitiesPanel）：登入／切換、登出（先確認）、停用、刪除。真的掛進 happy-dom，後端是 mock，
 * mock 的 WS 事件接到 store（`dispatchFrameForTest`），所以「登入完成之後這一列變已登入」「另一個分頁停用了它」走的是正式的事件路徑。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { dispatchFrameForTest, resetStoreForTest, useStore } from '../store/store'
import { IdentitiesPanel } from './IdentitiesPanel'

const mock = sharedMock
let stopEvents: (() => void) | null = null

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  stopEvents?.()
  stopEvents = null
  useStore.setState({ notices: [], shellView: null })
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
/** 設定面板裡 config 定義的那一列（不是從 shell 認出來的唯讀列）。 */
const row = (name: string) =>
  [...document.querySelectorAll('.identity-row:not(.is-shell)')].find((r) => r.querySelector('.identity-name')?.textContent?.includes(name))!
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent === text)
const chip = (name: string) => row(name).querySelector('.identity-login')?.textContent ?? ''
const calls = (requests: FakeRequest[], re: RegExp) => requests.filter((r) => r.method === 'POST' && re.test(r.path))

async function open() {
  const requests = mockApi(mock)
  stopEvents = mock.openSocket({ since: () => 0, onStatus: () => {}, onFrame: (f) => dispatchFrameForTest(f) })
  await advanceMockTime(200) // openSocket 要等 120ms 才算連上，之前的事件不會送
  await useStore.getState().refreshState()
  await useStore.getState().loadIdentityPrefs()
  await mount(<IdentitiesPanel />)
  return requests
}

it('登入：按「登入」開臨時 pane、切到那個 shell，事件進來後這一列變「已登入」並多出「切換」「登出」', async () => {
  const requests = await open()
  assert.match(chip('cc1'), /未登入/)
  assert.equal(btn(row('cc1'), '登出'), undefined, '沒登入就沒有登出鍵')
  await click(btn(row('cc1'), '登入')!)
  await until(() => calls(requests, /\/hosts\/local\/identities\/cc1\/login/).length === 1, '打了 login')
  await until(() => useStore.getState().shellView !== null, '切到登入用的 shell')
  await until(() => /已登入/.test(chip('cc1')), '收到 host_changed 後變已登入')
  assert.ok(btn(row('cc1'), '切換'), '已登入的列按鈕變「切換」')
  assert.ok(btn(row('cc1'), '登出'))
  assert.equal(btn(row('cc1'), '切換')!.disabled, false, 'busy 已經放掉')
})

it('登出：先跳確認、確認前不送；確認後送 logout，這一列變「未登入」、登出鍵消失', async () => {
  await mock.request('POST', '/hosts/local/identities/cc1/login') // 不依賴前一個測試：自己先把它登入
  const requests = await open()
  await until(() => /已登入/.test(chip('cc1')), '已登入')
  await click(btn(row('cc1'), '登出')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.ok(dialog)
  assert.match(dialog.textContent ?? '', /登出 cc1/)
  assert.equal(calls(requests, /logout/).length, 0, '還沒確認不能送')
  await click(btn(dialog, '登出')!)
  await until(() => calls(requests, /\/identities\/cc1\/logout/).length === 1, '打了 logout')
  await until(() => /未登入/.test(chip('cc1')), '變未登入')
  assert.equal(btn(row('cc1'), '登出'), undefined)
  assert.ok(btn(row('cc1'), '登入'))
})

it('登出：取消不送請求', async () => {
  const requests = await open()
  await click(btn(row('cc1'), '登入')!)
  await until(() => /已登入/.test(chip('cc1')), '先登入')
  useStore.setState({ shellView: null })
  await click(btn(row('cc1'), '登出')!)
  await click(btn(document.querySelector('[role=alertdialog]')!, '取消')!)
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  assert.equal(calls(requests, /logout/).length, 0)
  assert.match(chip('cc1'), /已登入/)
})

it('登入失敗（daemon 回 502）：跳錯誤通知、不切 shell、按鈕回到可按', async () => {
  await open()
  // 讓請求通過正式的 fetch／HttpTransport 錯誤解析路徑；直接回 HTTP 502，不讓 mock 的延遲 timer 影響這條錯誤測試。
  const requests = mockApi({
    request: async (method, path, body) => {
      if (method === 'POST' && /^\/hosts\/[^/]+\/identities\/cc1\/login$/.test(path)) {
        throw { status: 502, body: { error: 'bad_gateway', message: 'herdr 沒回應' } }
      }
      return mock.request(method, path, body)
    },
    upload: (path, file, opts) => mock.upload(path, file, opts),
  })
  // 先登出，才有「登入」鍵可以按。
  await mock.request('POST', '/hosts/local/identities/cc1/logout')
  await useStore.getState().refreshState()
  await until(() => /未登入/.test(chip('cc1')), '回到未登入')
  useStore.setState({ shellView: null })
  await click(btn(row('cc1'), '登入')!)
  assert.equal(calls(requests, /login/).length, 1, '按登入後確實送出一筆登入請求')
  await until(
    () => useStore.getState().notices.some((n) => n.kind === 'error') || useStore.getState().shellView !== null,
    '登入請求有結果',
  )
  assert.ok(useStore.getState().notices.some((n) => n.kind === 'error'), 'HTTP 502 要跳錯誤通知')
  assert.equal(useStore.getState().shellView, null)
  assert.equal(btn(row('cc1'), '登入')!.disabled, false)
  assert.equal(calls(requests, /login/).length, 1, '登入請求走到 mock daemon，收到 HTTP 502')
})

it('停用／啟用：本機按下去列上出現「停用」標記；另一個分頁的停用（WS 事件）也會跟著變', async () => {
  const requests = await open()
  const chipOf = () => row('cc1').querySelector('.identity-disabled-chip')
  assert.equal(chipOf(), null)
  await click(btn(row('cc1'), '停用')!)
  await until(() => chipOf() !== null, '本機停用後出現標記')
  assert.ok(requests.some((r) => r.method === 'PUT' && /\/identities\/cc1\/disabled/.test(r.path)))
  assert.ok(btn(row('cc1'), '啟用'), '按鈕變「啟用」')
  // 另一個分頁把它啟用：這個分頁只靠 identity_prefs_changed 事件更新。
  await mock.request('PUT', '/identities/cc1/disabled', { kind: 'claude', disabled: false, host: 'local' })
  await until(() => chipOf() === null, '另一個分頁啟用後標記消失')
  assert.ok(btn(row('cc1'), '停用'))
})

it('新增身份後出現在清單；用不到就能刪（要先確認）', async () => {
  const requests = await open()
  await mock.request('POST', '/identities', { name: 'dom3id', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-dom3' } })
  await useStore.getState().refreshState()
  await until(() => Boolean(row('dom3id')), '新身份出現')
  await click(row('dom3id').querySelector('button[aria-label^="刪除身份"]')!)
  assert.equal(requests.some((r) => r.method === 'DELETE'), false)
  await click(btn(document.querySelector('[role=alertdialog]')!, '刪除身份')!)
  await until(() => requests.some((r) => r.method === 'DELETE' && /\/identities\/dom3id/.test(r.path)), '送了 DELETE')
  await until(() => !row('dom3id'), '清單移除')
})

it('新增身份表單：env 打錯的行逐行說明並鎖住送出（不靜靜丟掉）；打了名字沒送出就關分頁會被攔', async () => {
  const requests = await open()
  const form = () => document.querySelector('.identities-panel form, form')!
  const nameInput = [...document.querySelectorAll<HTMLInputElement>('form input[type=text]')].find((i) => i.placeholder === 'cc1')!
  const envBox = document.querySelector<HTMLTextAreaElement>('form textarea')!
  const unload = () => {
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    return ev.defaultPrevented
  }
  assert.equal(unload(), false, '還沒打東西')
  await typeInto(nameInput, 'cc9')
  assert.equal(unload(), true, '打了名字沒送出：攔')
  await typeInto(envBox, 'CLAUDE_CONFIG_DIR ~/.claude-cc9')
  assert.match(form().textContent ?? '', /第 1 行.*沒有 =/)
  const submit = [...form().querySelectorAll('button')].find((b) => b.textContent === '新增身份') as HTMLButtonElement
  assert.equal(submit.disabled, true)
  assert.equal(calls(requests, /\/identities$/).length, 0, '一個請求都沒送')
})

it('新增身份表單：還沒命名時改了 env 也會攔 beforeunload', async () => {
  await open()
  const envBox = document.querySelector<HTMLTextAreaElement>('.identities-panel form textarea')!
  const unload = () => {
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    return ev.defaultPrevented
  }
  assert.equal(unload(), false, '初始表單沒有未儲存值')
  await typeInto(envBox, 'CLAUDE_CONFIG_DIR=/tmp/not-a-real-config')
  assert.equal(unload(), true, '環境值是使用者輸入，即使名稱還空白也不能無提示丟掉')
})

it('新增身份完成時不清掉送出後又輸入的下一個身份名稱', async () => {
  const requests = await open()
  const originalFetch = globalThis.fetch
  let release!: () => void
  let markStarted!: () => void
  const held = new Promise<void>((resolve) => { release = resolve })
  const started = new Promise<void>((resolve) => { markStarted = resolve })
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    if (String(input) === '/api/identities' && init?.method === 'POST') {
      markStarted()
      await held
    }
    return originalFetch(input, init)
  }) as unknown as typeof fetch
  try {
    const name = [...document.querySelectorAll<HTMLInputElement>('.identities-panel form input[type=text]')].find((i) => i.placeholder === 'cc1')!
    await typeInto(name, 'cc10')
    await click([...document.querySelectorAll('.identities-panel form button')].find((b) => b.textContent === '新增身份')!)
    await started
    await typeInto(name, 'cc11')
    release()
    await until(() => requests.some((r) => r.method === 'POST' && r.path === '/api/identities'), '新增身份完成')
    await until(() => Boolean(row('cc10')), '新身份出現在清單')
    assert.equal(name.value, 'cc11', '完成先前請求時不能覆蓋使用者後打的字')
  } finally {
    release()
    globalThis.fetch = originalFetch
  }
})
