import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types'
import { MockTransport } from '../api/mock'
import { click, keydown, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { dispatchFrameForTest, resetStoreForTest, useStore } from '../store/store'
import { IdentitiesPanel } from './IdentitiesPanel'

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
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent === text)
const copyButton = () => btn(document, '從本機複製身份')
const pickHost = async (name: string) => click(document.querySelector<HTMLElement>(`#identity-tab-${name}`)!)
const dialog = () => document.querySelector<HTMLElement>('.copy-identities-dialog')
const rowOf = (name: string) => document.querySelector<HTMLElement>(`.copy-identities-row[data-copy-identity="${name}"]`)
const checkboxOf = (name: string) => rowOf(name)?.querySelector<HTMLInputElement>('input[type=checkbox]') ?? null
const syncCalls = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/identities\/sync$/.test(r.path))

async function open(backend: MockTransport = sharedMock) {
  const requests = mockApi(backend)
  stopEvents = backend.openSocket({ since: () => 0, onStatus: () => {}, onFrame: (f) => dispatchFrameForTest(f) })
  await advanceMockTime(200)
  await useStore.getState().refreshState()
  await useStore.getState().loadIdentityPrefs()
  await mount(<IdentitiesPanel />)
  return requests
}

class EmptyPlan extends MockTransport {
  override async request(method: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH', path: string, body?: unknown): Promise<unknown> {
    if (method === 'GET' && /\/identities\/sync-plan$/.test(path)) {
      return { host: 'm4p', remote_shell: null, remote_rc: null, already_synced: 3, items: [] }
    }
    return super.request(method, path, body)
  }
}

class BrokenPlan extends MockTransport {
  override async request(method: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH', path: string, body?: unknown): Promise<unknown> {
    if (method === 'GET' && /\/identities\/sync-plan$/.test(path)) {
      throw new ApiError(502, { error: 'upstream', message: 'ssh 逾時' }, 'upstream')
    }
    return super.request(method, path, body)
  }
}

async function remoteBackend(backend: MockTransport = new MockTransport()): Promise<MockTransport> {
  resetStoreForTest()
  await backend.request('POST', '/hosts', { name: 'm4p', ssh: 'm4p' })
  return backend
}

it('遠端分頁才出現「從本機複製身份」；離線時 disabled', async () => {
  const backend = await remoteBackend()
  await open(backend)
  assert.equal(copyButton(), undefined, '本機分頁不出現')
  await pickHost('m4p')
  assert.ok(copyButton(), '遠端分頁出現')
  assert.equal(copyButton()!.disabled, false)
  backend.setHostConnected('m4p', false)
  await until(() => copyButton()?.disabled === true, '離線後按鈕 disabled')
})

it('對話框載入預覽：缺漏的預勾、設定不同的 disabled，送出只送所選名稱', async () => {
  const backend = await remoteBackend()
  const requests = await open(backend)
  await pickHost('m4p')
  await click(copyButton()!)
  await until(() => rowOf('ccdev') !== null, '載入 sync-plan')
  assert.equal(checkboxOf('ccdev')!.checked, true, '缺漏且可套用：預勾')
  assert.equal(checkboxOf('cc2')!.disabled, true, '設定不同：disabled')
  assert.equal(checkboxOf('cc2')!.checked, false)
  assert.match(rowOf('cc2')!.textContent ?? '', /設定不同/)
  assert.ok(dialog()!.textContent?.includes('已同步 2 項'), '其餘已同步的項目數')
  const submit = btn(dialog()!, '建立所選身份（1）')!
  await click(submit)
  await until(() => syncCalls(requests).length === 1, '送出 POST sync')
  assert.deepEqual(syncCalls(requests)[0].body, { names: ['ccdev'], dry_run: false })
  await until(() => document.querySelector('[data-result="ccdev"]') !== null, '顯示結果')
  assert.match(document.querySelector('[data-result="ccdev"]')!.textContent ?? '', /已建立/)
  const login = btn(document.querySelector('[data-result="ccdev"]')!, '登入')
  assert.ok(login, '已建立的身份旁有登入按鈕')
  await click(login!)
  await until(() => document.querySelector('.copy-identities-result') !== null, '保持結果畫面')
  assert.ok(
    requests.some((r) => r.method === 'POST' && r.path === '/api/hosts/m4p/identities/ccdev/login'),
    '登入走既有的 identity login 流程',
  )
})

it('沒有可複製項目：顯示已擁有所有身份與同步數量', async () => {
  await open(await remoteBackend(new EmptyPlan()))
  await pickHost('m4p')
  await click(copyButton()!)
  await until(() => dialog()?.textContent?.includes('已擁有本機的所有可複製身份') === true, '空清單訊息')
  assert.ok(dialog()!.textContent?.includes('已同步 3 項'))
  assert.equal(btn(dialog()!, '建立所選身份（0）'), undefined, '沒有項目就沒有送出鍵')
})

it('載入失敗：顯示錯誤與重新載入', async () => {
  await open(await remoteBackend(new BrokenPlan()))
  await pickHost('m4p')
  await click(copyButton()!)
  await until(() => dialog()?.querySelector('[role=alert]') !== null, '錯誤提示')
  assert.ok(dialog()!.textContent?.includes('無法載入可複製的身份'))
  assert.ok(btn(dialog()!, '重新載入'))
})

it('Escape 關閉對話框', async () => {
  const backend = await remoteBackend()
  await open(backend)
  await pickHost('m4p')
  await click(copyButton()!)
  await until(() => dialog() !== null, '對話框開啟')
  await keydown(document.activeElement!, 'Escape')
  await until(() => dialog() === null, 'Escape 關閉')
})
