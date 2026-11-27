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
const row = (name: string) =>
  [...document.querySelectorAll('.identity-row:not(.is-shell)')].find((r) => r.querySelector('.identity-name')?.textContent?.includes(name))!
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent === text)
const chip = (name: string) => row(name).querySelector('.identity-login')?.textContent ?? ''
const calls = (requests: FakeRequest[], re: RegExp) => requests.filter((r) => r.method === 'POST' && re.test(r.path))
const shellRow = (host: string, identity: string) =>
  document.querySelector<HTMLElement>(`.identity-row.is-shell[data-shell-host="${host}"][data-shell-identity="${identity}"]`)
const shellDeleteButton = (host: string, identity: string) =>
  shellRow(host, identity)?.querySelector<HTMLButtonElement>('button[aria-label*="shell alias"]')
const pickHost = async (name: string) => click(document.querySelector<HTMLElement>(`#identity-tab-${name}`)!)
const shellDeleteCalls = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'DELETE' && r.path.includes('/shell-identities/'))

async function open(backend: MockTransport = mock) {
  const requests = mockApi(backend)
  stopEvents = backend.openSocket({ since: () => 0, onStatus: () => {}, onFrame: (f) => dispatchFrameForTest(f) })
  await advanceMockTime(200)
  await useStore.getState().refreshState()
  await useStore.getState().loadIdentityPrefs()
  await mount(<IdentitiesPanel />)
  return requests
}

it('登入：按「登入」開臨時 pane、切到那個 shell，事件進來後這一列變「已登入」並多出「切換」「登出」', async () => {
  const requests = await open()
  assert.match(chip('cc1'), /未登入/)
  assert.equal(btn(row('cc1'), '登出此帳號…'), undefined, '沒登入就沒有登出鍵')
  await click(btn(row('cc1'), '登入')!)
  await until(() => calls(requests, /\/hosts\/local\/identities\/cc1\/login/).length === 1, '打了 login')
  await until(() => useStore.getState().shellView !== null, '切到登入用的 shell')
  await until(() => /已登入/.test(chip('cc1')), '收到 host_changed 後變已登入')
  assert.ok(btn(row('cc1'), '重新登入…'), '已登入的列按鈕變「重新登入…」')
  assert.ok(btn(row('cc1'), '登出此帳號…'))
  assert.equal(btn(row('cc1'), '重新登入…')!.disabled, false, 'busy 已經放掉')
})

it('登出：先跳確認、確認前不送；確認後送 logout，這一列變「未登入」、登出鍵消失', async () => {
  await mock.request('POST', '/hosts/local/identities/cc1/login')
  const requests = await open()
  await until(() => /已登入/.test(chip('cc1')), '已登入')
  await click(btn(row('cc1'), '登出此帳號…')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.ok(dialog)
  assert.match(dialog.textContent ?? '', /登出：.*cc1/)
  assert.equal(calls(requests, /logout/).length, 0, '還沒確認不能送')
  await click(btn(dialog, '登出')!)
  await until(() => calls(requests, /\/identities\/cc1\/logout/).length === 1, '打了 logout')
  await until(() => /未登入/.test(chip('cc1')), '變未登入')
  assert.equal(btn(row('cc1'), '登出此帳號…'), undefined)
  assert.ok(btn(row('cc1'), '登入'))
})

it('登出：取消不送請求', async () => {
  const requests = await open()
  await click(btn(row('cc1'), '登入')!)
  await until(() => /已登入/.test(chip('cc1')), '先登入')
  useStore.setState({ shellView: null })
  await click(btn(row('cc1'), '登出此帳號…')!)
  await click(btn(document.querySelector('[role=alertdialog]')!, '取消')!)
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  assert.equal(calls(requests, /logout/).length, 0)
  assert.match(chip('cc1'), /已登入/)
})

it('登入失敗（daemon 回 502）：跳錯誤通知、不切 shell、按鈕回到可按', async () => {
  const requests = await open()
  mock.failNext('POST', '^/hosts/[^/]+/identities/cc1/login$', 502, { error: 'bad_gateway', message: 'herdr 沒回應' })
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

it('不再提供給新 Bot／重新提供：本機按下去列上出現「不提供新 Bot」標記；另一個分頁的變更（WS 事件）也會跟著變', async () => {
  const requests = await open()
  const chipOf = () => row('cc1').querySelector('.identity-disabled-chip')
  assert.equal(chipOf(), null)
  await click(btn(row('cc1'), '不再提供給新 Bot')!)
  await until(() => chipOf() !== null, '本機停用後出現標記')
  assert.ok(requests.some((r) => r.method === 'PUT' && /\/identities\/cc1\/disabled/.test(r.path)))
  assert.ok(btn(row('cc1'), '重新提供給新 Bot'), '按鈕變「重新提供給新 Bot」')
  await mock.request('PUT', '/identities/cc1/disabled', { kind: 'claude', disabled: false, host: 'local' })
  await until(() => chipOf() === null, '另一個分頁啟用後標記消失')
  assert.ok(btn(row('cc1'), '不再提供給新 Bot'))
})

it('新增身份後出現在清單；用不到就能刪（要先確認）', async () => {
  const requests = await open()
  await mock.request('POST', '/identities', { name: 'dom3id', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-dom3' } })
  await useStore.getState().refreshState()
  await until(() => Boolean(row('dom3id')), '新身份出現')
  await click(btn(row('dom3id'), '移除身份設定…')!)
  assert.equal(requests.some((r) => r.method === 'DELETE'), false)
  await click(btn(document.querySelector('[role=alertdialog]')!, '移除身份設定')!)
  await until(() => requests.some((r) => r.method === 'DELETE' && /\/identities\/dom3id/.test(r.path)), '送了 DELETE')
  await until(() => !row('dom3id'), '清單移除')
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

const defaultRow = (host: string, kind: string) => document.querySelector(`[data-k="default-${host}-${kind}"]`)!

it('預設帳號：每個已安裝 kind 一列；登出先確認、確認後打 kinds/{kind}/logout 並切到那個 shell', async () => {
  const requests = await open()
  const row = defaultRow('local', 'claude')
  assert.ok(row, 'claude 預設帳號那一列')
  assert.match(row.textContent ?? '', /預設帳號/)
  await click(btn(row, '登出此帳號…')!)
  assert.equal(calls(requests, /\/kinds\/claude\/logout/).length, 0, '確認前不送')
  await click(btn(document.querySelector('[role=alertdialog]')!, '登出')!)
  await until(() => calls(requests, /\/hosts\/local\/kinds\/claude\/logout/).length === 1, '送出預設帳號登出')
  await until(() => useStore.getState().shellView !== null, '切到登出用的 shell')
  await until(() => /未登入/.test(defaultRow('local', 'claude').textContent ?? ''), '登出後變未登入')
})

it('預設帳號：從額度列隱藏要先確認、存成 @default 的停用；隱藏後顯示「已隱藏」，按「顯示回額度列」復原', async () => {
  const requests = await open()
  const put = () => requests.filter((r) => r.method === 'PUT' && /\/identities\/%40default\/disabled/.test(r.path))
  await click(btn(defaultRow('local', 'claude'), '從額度列隱藏')!)
  assert.equal(put().length, 0, '確認前不送')
  await click(btn(document.querySelector('[role=alertdialog]')!, '從額度列隱藏')!)
  await until(() => put().length === 1, '送出隱藏')
  assert.deepEqual(put()[0]!.body, { kind: 'claude', disabled: true, host: 'local' })
  await until(() => /已隱藏/.test(defaultRow('local', 'claude').textContent ?? ''), '列上標已隱藏')
  assert.ok(useStore.getState().disabledIdentities.includes('local|claude|@default'))
  await click(btn(defaultRow('local', 'claude'), '顯示回額度列')!)
  await until(() => put().length === 2 && !/已隱藏/.test(defaultRow('local', 'claude').textContent ?? ''), '復原')
  assert.deepEqual(put()[1]!.body, { kind: 'claude', disabled: false, host: 'local' })
})

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
  await click(btn(dialog, '從 shell 設定移除')!)
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
  await pickHost('m4p')
  assert.ok(shellRow('m4p', 'cc2'))
  await pickHost('lab')
  assert.ok(shellRow('lab', 'cc2'))
  await pickHost('local')
  assert.ok(row('cc2'), '同名 config identity row 存在')

  await pickHost('m4p')
  await click(shellDeleteButton('m4p', 'cc2')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  await click(btn(dialog, '從 shell 設定移除')!)
  const backupPath = '/mock/m4p/.agents-manager/shell-identity-backups/cc2.bak'
  await until(() => Boolean(document.querySelector('[role=status]')?.textContent?.includes(backupPath)), '顯示成功備份路徑')

  const [request] = shellDeleteCalls(requests)
  assert.equal(shellDeleteCalls(requests).length, 1)
  assert.equal(request.path, '/api/hosts/m4p/shell-identities/cc2?kind=claude')
  assert.equal(request.body, undefined, 'DELETE 不帶 body')
  assert.equal(shellRow('m4p', 'cc2'), null, '只移除指定 host 的 alias')
  await pickHost('lab')
  assert.ok(shellRow('lab', 'cc2'), '另一台同名 shell alias 保留')
  await pickHost('local')
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
    await click(btn(document.querySelector('[role=alertdialog]')!, '從 shell 設定移除')!)
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
    await pickHost('m4p')
    backend.failNext('DELETE', '^/hosts/m4p/shell-identities/cc2\\?kind=claude$', 409, {
      error: 'conflict', reason: 'identity_in_use', refs: [{ type, id: 'ref-id' }],
    })
    await click(shellDeleteButton('m4p', 'cc2')!)
    await click(btn(document.querySelector('[role=alertdialog]')!, '從 shell 設定移除')!)
    await until(() => Boolean(document.querySelector('.identity-shell-delete-result')), '顯示失敗目標')
    const text = document.querySelector('.identity-shell-delete-result')!.textContent ?? ''
    assert.ok(text.includes('主機：m4p；Claude shell alias：cc2'))
    assert.ok(text.includes(expected))
    assert.ok(shellRow('m4p', 'cc2'))
    await pickHost('local')
    assert.ok(shellRow('local', 'cc2'))
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
  await click(btn(document.querySelector('[role=alertdialog]')!, '從 shell 設定移除')!)
  await until(() => Boolean(document.querySelector('.identity-shell-delete-result')), '顯示備份未確認')
  const text = document.querySelector('.identity-shell-delete-result')!.textContent ?? ''
  assert.match(text, /無法確認備份是否建立/)
  assert.match(text, /備份路徑（未確認）/)
  assert.ok(text.includes(backup))
  assert.ok(!text.includes('保留的備份路徑：'))
  assert.ok(shellRow('local', 'cc2'))
})

const tabs = () => [...document.querySelectorAll<HTMLElement>('[role=tablist].identity-host-tabs [role=tab]')]
const kindCards = () => [...document.querySelectorAll<HTMLElement>('[data-kind-card]')].map((c) => c.dataset.kindCard)

it('只有一台主機時不顯示分頁；每種 CLI 各一張卡，卡頭寫出 CLI 名稱', async () => {
  resetStoreForTest()
  await open(new MockTransport())
  assert.equal(document.querySelector('[role=tablist].identity-host-tabs'), null, '只有本機就不顯示 tab')
  for (const kind of ['claude', 'codex', 'grok', 'agy']) {
    assert.ok(kindCards().includes(`local-${kind}`), `local 有 ${kind} 卡`)
    const card = document.querySelector<HTMLElement>(`[data-kind-card="local-${kind}"]`)!
    assert.equal(card.querySelector('.identity-kind-card-head')?.textContent?.includes(kind), true, `${kind} 卡頭有名稱`)
  }
  assert.ok(document.querySelector('[data-kind-card="local-claude"] button'), '本機卡有啟用開關')
})

it('多台主機：每台一個 tab，點哪台只列哪台的卡；遠端卡沒有本機的啟用開關', async () => {
  resetStoreForTest()
  const backend = new MockTransport()
  await backend.request('POST', '/hosts', { name: 'm4p', ssh: 'm4p' })
  await open(backend)
  assert.deepEqual(tabs().map((t) => t.id), ['identity-tab-local', 'identity-tab-m4p'])
  assert.equal(tabs()[0].getAttribute('aria-selected'), 'true', '預設是本機')
  await pickHost('m4p')
  assert.equal(tabs()[1].getAttribute('aria-selected'), 'true')
  assert.ok(kindCards().every((k) => k?.startsWith('m4p-')), `只剩 m4p 的卡：${kindCards().join(',')}`)
  const remoteHead = [...(document.querySelector('[data-kind-card="m4p-claude"] .identity-kind-card-head')?.querySelectorAll('button') ?? [])]
  assert.equal(remoteHead.filter((b) => b.textContent !== '新增帳號').length, 0, '遠端卡沒有停用開關（只有新增帳號）')
})

it('斷線主機的 tab 仍在、標「斷線」，內容顯示離線說明且沒有登入鍵', async () => {
  resetStoreForTest()
  const backend = new MockTransport()
  await backend.request('POST', '/hosts', { name: 'bad_lab', ssh: 'bad_lab' })
  await open(backend)
  await pickHost('bad_lab')
  const tab = document.querySelector('#identity-tab-bad_lab')!
  assert.match(tab.textContent ?? '', /斷線/)
  assert.ok(document.querySelector('.identity-offline-banner'), '離線說明')
  assert.equal(document.querySelector('[data-kind-card="bad_lab-claude"] .identity-login-btn'), null, '斷線時不給登入鍵')
})

it('記住上次選的主機分頁：重新掛載後還停在那一台', async () => {
  resetStoreForTest()
  const backend = new MockTransport()
  await backend.request('POST', '/hosts', { name: 'm4p', ssh: 'm4p' })
  await open(backend)
  await pickHost('m4p')
  await unmountAll()
  await mount(<IdentitiesPanel />)
  assert.equal(document.querySelector('#identity-tab-m4p')?.getAttribute('aria-selected'), 'true')
})

it('停用本機 CLI 要先確認，確認框說明本機受影響的 Bot 數；確認前不送請求', async () => {
  const requests = await open()
  const before = requests.length
  const card = document.querySelector<HTMLElement>('[data-kind-card="local-claude"]')!
  await click(btn(card, '停用本機 claude…')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.match(dialog.textContent ?? '', /目前本機有 \d+ 顆 claude Bot/)
  assert.equal(requests.slice(before).filter((r) => r.method !== 'GET').length, 0, '確認前不送')
  await click(btn(dialog, '停用本機 claude')!)
  await until(() => requests.some((r) => r.method === 'PUT' && /\/kinds\/claude\/disabled/.test(r.path)), '確認後送出停用')
})

it('cc0 設定檔與預設帳號分開標示：同名不合併', async () => {
  await open()
  assert.match(row('cc0').textContent ?? '', /cc0 設定檔，與預設帳號不同目錄/)
  assert.match(defaultRow('local', 'claude').textContent ?? '', /預設帳號（未指定身份的 Bot 使用；不是 cc0 設定檔）/)
})

const openAddFromHeader = async () => click([...document.querySelectorAll<HTMLButtonElement>('.identity-page-actions button')][0]!)
const dialog = () => document.querySelector<HTMLElement>('.add-identity-dialog')!
const dialogButton = (text: string) => [...dialog().querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === text)!
const dialogName = () => dialog().querySelector<HTMLInputElement>('input[type=text]')!
const dialogEnv = () => dialog().querySelector<HTMLTextAreaElement>('textarea')!
const pageText = () => document.querySelector('.identities-panel')?.textContent ?? ''

it('新增帳號對話框：env 打錯的行逐行說明並鎖住送出；一個請求都沒送', async () => {
  const requests = await open()
  await openAddFromHeader()
  assert.ok(dialog(), '頁首打開對話框')
  await typeInto(dialogName(), 'cc9')
  await typeInto(dialogEnv(), 'CLAUDE_CONFIG_DIR ~/.claude-cc9')
  assert.match(dialog().textContent ?? '', /第 1 行.*沒有 =/)
  assert.equal(dialogButton('新增並登入').disabled, true)
  assert.equal(calls(requests, /\/identities$/).length, 0)
})

it('從 codex 卡按「新增帳號」：預選 codex、env 預填 CODEX_HOME＝$HOME/.codex-<名稱>', async () => {
  await open()
  await click(document.querySelector<HTMLButtonElement>('[data-kind-card="local-codex"] .identity-kind-actions button')!)
  const picked = dialog().querySelector<HTMLInputElement>('input[name=add-identity-kind]:checked')
  assert.equal(picked?.value, 'codex')
  await typeInto(dialogName(), 'work')
  assert.equal(dialogEnv().value, 'CODEX_HOME=$HOME/.codex-work')
  assert.ok(dialog().textContent?.includes('$HOME/.codex-work'), '摘要顯示實際目錄')
})

it('新增 agy 身份：預填 HOME=$HOME/.agy-，送出的是 kind agy＋HOME（建立於本機，不帶 host）', async () => {
  const requests = await open()
  await openAddFromHeader()
  await click(dialog().querySelector<HTMLInputElement>('input[value=agy]')!)
  assert.equal(dialogEnv().value, 'HOME=$HOME/.agy-')
  await typeInto(dialogName(), 'agb')
  await click(dialogButton('新增並登入'))
  await until(() => requests.some((r) => r.method === 'POST' && r.path === '/api/identities'), '送出新增身份')
  const post = requests.find((r) => r.method === 'POST' && r.path === '/api/identities')!
  assert.deepEqual(post.body, { name: 'agb', kind: 'agy', env: { HOME: '$HOME/.agy-agb' } })
})

it('新增後：標亮新列、顯示尚未登入＋立即登入；立即登入是背景開，不切走設定頁', async () => {
  const requests = await open()
  await openAddFromHeader()
  await typeInto(dialogName(), 'cc12')
  await click(dialogButton('新增並登入'))
  await until(() => Boolean(document.querySelector('[data-fresh-row="true"]')), '新列標亮')
  await act(() =>
    useStore.setState((s) => ({
      localIdentityStatus: { ...s.localIdentityStatus, cc12: { ...s.localIdentityStatus.cc12, logged_in: false } },
    })),
  )
  await until(() => pageText().includes('尚未登入'), '提示尚未登入')
  await click(dialogButton('稍後再登入'))
  assert.equal(document.querySelector('.add-identity-dialog'), null)
  await click([...document.querySelectorAll<HTMLButtonElement>('.identity-fresh-banner button')].find((b) => b.textContent === '立即登入')!)
  await until(() => requests.some((r) => /\/identities\/cc12\/login/.test(r.path)), '送了 login')
  assert.equal(useStore.getState().shellView, null, '背景登入：不切到 shell')
  assert.ok(useStore.getState().identityLoginPane, '登入終端記在 identityLoginPane')
  assert.ok(document.querySelector('.identities-panel'), '設定頁的身份面板還在')
})

it('新增對話框：開啟時清掉別的身份留下的登入終端，「打開登入終端」只切到這次建立的身份', async () => {
  await open(new MockTransport())
  useStore.setState({ identityLoginPane: { host: 'local', identity: 'cc1', paneId: 'stale-pane', cwd: '' } })
  await openAddFromHeader()
  assert.equal(useStore.getState().identityLoginPane, null, '開對話框就清掉別的身份的登入終端')
  await typeInto(dialogName(), 'cc14')
  await click(dialogButton('新增並登入'))
  await until(() => useStore.getState().identityLoginPane?.identity === 'cc14', '登入終端記成這次的身份')
  await until(() => Boolean(dialogButton('打開登入終端')), '出現打開登入終端')
  await click(dialogButton('打開登入終端'))
  const pane = useStore.getState().identityLoginPane!
  assert.equal(useStore.getState().shellView?.paneId, pane.paneId)
  assert.notEqual(pane.paneId, 'stale-pane')
})
