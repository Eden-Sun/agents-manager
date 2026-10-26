/**
 * 環境設定的「身份」面板（IdentitiesPanel）：登入／切換、登出（先確認）、停用、刪除。真的掛進 happy-dom，後端是 mock，
 * mock 的 WS 事件接到 store（`dispatchFrameForTest`），所以「登入完成之後這一列變已登入」「另一個分頁停用了它」走的是正式的事件路徑。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from '../api/mock'
import { act, click, mockApi, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
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
const shellRow = (host: string, identity: string) =>
  document.querySelector<HTMLElement>(`.identity-row.is-shell[data-shell-host="${host}"][data-shell-identity="${identity}"]`)
const shellDeleteButton = (host: string, identity: string) =>
  shellRow(host, identity)?.querySelector<HTMLButtonElement>('button[aria-label*="shell alias"]')
const shellDeleteCalls = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'DELETE' && r.path.includes('/shell-identities/'))

async function open(backend: MockTransport = mock) {
  const requests = mockApi(backend)
  stopEvents = backend.openSocket({ since: () => 0, onStatus: () => {}, onFrame: (f) => dispatchFrameForTest(f) })
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
  const requests = await open()
  mock.failNext('POST', '^/hosts/[^/]+/identities/cc1/login$', 502, { error: 'bad_gateway', message: 'herdr 沒回應' })
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

it('config 裡既有的空值 env 身份在列上標警告（只標示，不送任何改寫請求）；有值的身份不標', async () => {
  const requests = await open()
  await mount_with_identities()
  assert.equal(row('cc1').querySelector('.identity-empty-env'), null, '有值的身份不標')
  const warn = row('cc-empty').querySelector('.identity-empty-env')
  assert.ok(warn, '空值身份要有警告')
  assert.match(warn!.textContent ?? '', /CLAUDE_CONFIG_DIR.*預設帳號/)
  assert.deepEqual(requests.filter((r) => r.method !== 'GET' && /identit/.test(r.path)), [], '設定頁只標示，不改寫')
})

async function mount_with_identities() {
  const { identities } = useStore.getState()
  await act(() => useStore.setState({ identities: [...identities, { name: 'cc-empty', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '' }, args: [] }] }))
}

it('shell alias 移除：取消確認不送請求；確認框點名主機與 alias 並說明保留資料和外部編輯器窗口', async () => {
  const requests = await open()
  const remove = shellDeleteButton('local', 'cc2')
  assert.ok(remove, 'Claude ccN shell alias 有移除按鈕')
  await click(remove!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.match(dialog.textContent ?? '', /本機.*cc2/)
  assert.match(dialog.textContent ?? '', /登入憑證、設定目錄與對話歷史.*保留/)
  assert.match(dialog.textContent ?? '', /外部編輯器可能.*檔案替換之間修改設定/)
  assert.match(dialog.textContent ?? '', /備份.*不會自動刪除/)
  await click(btn(dialog, '取消')!)
  assert.equal(shellDeleteCalls(requests).length, 0, '取消不能送 DELETE')
  assert.ok(shellRow('local', 'cc2'), '取消後 alias row 保留')
})

it('shell alias 移除被新 ref 擋下：row 保留、refs 與已建立的備份路徑可見', async () => {
  const requests = await open()
  const backupPath = '/mock/local/.agents-manager/shell-identity-backups/cc2.bak'
  mock.failNext('DELETE', '^/hosts/local/shell-identities/cc2\\?kind=claude$', 409, {
    error: 'conflict', reason: 'alias_new_reference', message: 'a new reference appeared',
    host: 'local', kind: 'claude', identity: 'cc2', backup_path: backupPath,
    refs: [{ type: 'bot', id: 'bot-new' }], restored: true,
  })
  await click(shellDeleteButton('local', 'cc2')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  await click(btn(dialog, '移除 alias')!)
  await until(() => Boolean(document.querySelector('[role=alert]')?.textContent?.includes(backupPath)), '顯示備份路徑')
  const result = document.querySelector('[role=alert]')!
  assert.match(result.textContent ?? '', /刪除期間有參照開始使用/)
  assert.match(result.textContent ?? '', /Bot bot-new/)
  assert.ok(result.textContent?.includes(backupPath))
  assert.ok(shellRow('local', 'cc2'), '被 ref 擋下後 row 仍在')
  assert.equal(shellDeleteCalls(requests).length, 1)
})

it('shell alias 移除使用 host/kind/name 專用 URL；只移除 m4p alias，保留另一台同名 alias 與 config identity', async () => {
  resetStoreForTest()
  const backend = new MockTransport()
  await backend.request('POST', '/identities', {
    name: 'cc2', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-config-cc2' },
  })
  await backend.request('POST', '/hosts', { name: 'm4p', ssh: 'm4p' })
  await backend.request('POST', '/hosts', { name: 'lab', ssh: 'lab' })
  const requests = await open(backend)
  assert.ok(shellRow('m4p', 'cc2'))
  assert.ok(shellRow('lab', 'cc2'))
  assert.ok(row('cc2'), '同名 config identity row 存在')

  await click(shellDeleteButton('m4p', 'cc2')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  await click(btn(dialog, '移除 alias')!)
  const backupPath = '/mock/m4p/.agents-manager/shell-identity-backups/cc2.bak'
  await until(() => Boolean(document.querySelector('[role=status]')?.textContent?.includes(backupPath)), '顯示成功備份路徑')

  const [request] = shellDeleteCalls(requests)
  assert.equal(shellDeleteCalls(requests).length, 1)
  assert.equal(request.path, '/api/hosts/m4p/shell-identities/cc2?kind=claude')
  assert.equal(request.body, undefined, 'DELETE 不帶 body')
  assert.equal(shellRow('m4p', 'cc2'), null, '只移除指定 host 的 alias')
  assert.ok(shellRow('lab', 'cc2'), '另一台同名 shell alias 保留')
  assert.ok(row('cc2'), '同名 config identity 的舊 DELETE 資料不受影響')
  assert.equal(shellDeleteCalls(requests).length, 1, '不呼叫 config identity DELETE')
  assert.match(document.querySelector('[role=status]')?.textContent ?? '', /外部編輯器可能.*檔案替換之間修改設定/)
})

for (const [label, reply] of [
  ['未確認成功', { ok: false, host: 'local', kind: 'claude', identity: 'cc2', backup_path: '/mock/retained/cc2.bak', warning: 'external_editor_window' }],
  ['缺少備份路徑', { ok: true, host: 'local', kind: 'claude', identity: 'cc2', warning: 'external_editor_window' }],
  ['主機不符', { ok: true, host: 'other', kind: 'claude', identity: 'cc2', backup_path: '/mock/other/cc2.bak', warning: 'external_editor_window' }],
  ['備份路徑非絕對', { ok: true, host: 'local', kind: 'claude', identity: 'cc2', backup_path: 'relative/cc2.bak', warning: 'external_editor_window' }],
] as const) {
  it(`shell alias 移除回應${label}：不得顯示成功，保留已回傳備份路徑`, async () => {
    resetStoreForTest()
    const backend = new MockTransport()
    const original = backend.request.bind(backend)
    backend.request = async (method, path, body) =>
      method === 'DELETE' && path.includes('/shell-identities/') ? reply : original(method, path, body)
    await open(backend)
    await click(shellDeleteButton('local', 'cc2')!)
    await click(btn(document.querySelector('[role=alertdialog]')!, '移除 alias')!)
    await until(() => Boolean(document.querySelector('.identity-shell-delete-result')), '回應已呈現')
    const result = document.querySelector('.identity-shell-delete-result')!
    assert.equal(result.getAttribute('role'), 'alert', '未知或不符契約的回應不能宣稱成功')
    assert.match(result.textContent ?? '', /無法確認.*移除結果/)
    if ('backup_path' in reply) {
      assert.ok(result.textContent?.includes(reply.backup_path), '已回傳的備份路徑仍可見')
      assert.match(result.textContent ?? '', /備份路徑（未確認）/)
    } else {
      assert.match(result.textContent ?? '', /未收到備份路徑/)
    }
    assert.ok(shellRow('local', 'cc2'), '不能依不確定回應清掉原列')
  })
}

for (const [type, expected] of [
  ['bot', 'Bot ref-id'], ['runtime', '執行中的 Run ref-id'],
  ['supervisor', '巡檢 ref-id'], ['supervisor_role', 'Supervisor role ref-id'],
] as const) {
  it(`shell alias ref ${type}：失敗標示精確參照與目標主機`, async () => {
    resetStoreForTest()
    const backend = new MockTransport()
    await backend.request('POST', '/hosts', { name: 'm4p', ssh: 'm4p' })
    await open(backend)
    backend.failNext('DELETE', '^/hosts/m4p/shell-identities/cc2\\?kind=claude$', 409, {
      error: 'conflict', reason: 'identity_in_use', refs: [{ type, id: 'ref-id' }],
    })
    await click(shellDeleteButton('m4p', 'cc2')!)
    await click(btn(document.querySelector('[role=alertdialog]')!, '移除 alias')!)
    await until(() => Boolean(document.querySelector('.identity-shell-delete-result')), '顯示失敗目標')
    const text = document.querySelector('.identity-shell-delete-result')!.textContent ?? ''
    assert.ok(text.includes('主機：m4p；Claude shell alias：cc2'))
    assert.ok(text.includes(expected))
    assert.ok(shellRow('local', 'cc2'))
    assert.ok(shellRow('m4p', 'cc2'))
  })
}

it('同主機 config/shell 同名：保留既有 config 入口，不以 shell DELETE 代替', async () => {
  resetStoreForTest()
  const backend = new MockTransport()
  await backend.request('POST', '/identities', { name: 'cc2', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc2' } })
  const requests = await open(backend)
  assert.ok(row('cc2'))
  assert.equal(shellRow('local', 'cc2'), null)
  assert.equal(shellDeleteButton('local', 'cc2'), undefined)
  assert.equal(shellDeleteCalls(requests).length, 0)
})

it('備份建立未確認：回傳預定位置但不能宣稱備份已保留', async () => {
  await open()
  const backup = '/mock/local/intended-backup.bak'
  mock.failNext('DELETE', '^/hosts/local/shell-identities/cc2\\?kind=claude$', 409, {
    error: 'conflict', reason: 'alias_backup_unconfirmed', backup_path: backup,
  })
  await click(shellDeleteButton('local', 'cc2')!)
  await click(btn(document.querySelector('[role=alertdialog]')!, '移除 alias')!)
  await until(() => Boolean(document.querySelector('.identity-shell-delete-result')), '顯示備份未確認')
  const text = document.querySelector('.identity-shell-delete-result')!.textContent ?? ''
  assert.match(text, /無法確認備份是否建立/)
  assert.match(text, /備份路徑（未確認）/)
  assert.ok(text.includes(backup))
  assert.ok(!text.includes('保留的備份路徑：'))
  assert.ok(shellRow('local', 'cc2'))
})
