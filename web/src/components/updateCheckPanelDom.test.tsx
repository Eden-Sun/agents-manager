/**
 * 環境設定「更新檢查」（UpdateCheckPanel，#1772）：每台主機一張表，herdr／各 agent CLI／herdr integration 都在。
 * 真的掛進 happy-dom，後端是 mock。integration 的更新只在按下「更新…」並確認後才送 POST；沒裝的、已是最新的沒有按鈕。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { UpdateCheckPanel } from './UpdateCheckPanel'
import type { ToolMap } from '../api/types'

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  useStore.setState({ notices: [] })
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const row = (id: string) => document.querySelector<HTMLElement>(`[data-row="${id}"]`)
const text = (id: string) => row(id)?.textContent ?? ''

async function open(): Promise<FakeRequest[]> {
  const requests = mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({
    upstreamUpdates: {
      herdr: { kind: 'herdr', latest: '0.9.5', target: '0.9.5', hasUpdate: true, hosts: [{ host: 'local', installedVersion: 'herdr 0.9.1', error: null, behind: true }], notify: null, text: null },
    },
  })
  await mount(<UpdateCheckPanel />)
  await until(() => row('local:integration-claude') !== null, 'integration 列出現')
  return requests
}

it('列出 herdr、各 agent CLI 與四家 integration，狀態各自標出', async () => {
  await open()
  assert.match(text('local:herdr-cli'), /herdr CLI.*0\.9\.1.*0\.9\.5.*可更新/)
  assert.ok(row('local:cli-claude') && row('local:cli-codex') && row('local:cli-grok') && row('local:cli-agy'), '四家 CLI 都有一列')
  assert.match(text('local:integration-claude'), /v8.*v11.*可更新/)
  assert.match(text('local:integration-codex'), /v8.*v10.*可更新/)
  assert.match(text('local:integration-antigravity-cli'), /未安裝/)
  assert.match(text('local:integration-grok'), /未安裝/)
})

it('沒裝的 integration 沒有按鈕；落後的 integration 按「更新…」確認後才送 POST，裝完那列變最新', async () => {
  const requests = await open()
  assert.equal(row('local:integration-grok')!.querySelector('button'), null, '沒裝的只顯示')
  const post = () => requests.filter((r) => r.method === 'POST' && /herdr-integration$/.test(r.path))
  await click(row('local:integration-claude')!.querySelector('button')!)
  assert.equal(post().length, 0, '還沒確認，不能送')
  const confirm = [...document.querySelectorAll('button')].find((b) => b.textContent === '更新' && b.closest('[role="alertdialog"]'))
  assert.ok(confirm, '確認框出現')
  await click(confirm!)
  await until(() => post().length === 1, '送了 POST')
  assert.deepEqual(post()[0].body, { name: 'claude' })
  assert.match(post()[0].path, /\/hosts\/local\/herdr-integration$/)
  await until(() => /最新/.test(text('local:integration-claude')), '那列變成最新')
  assert.equal(row('local:integration-claude')!.querySelector('button'), null)
  assert.ok(row('local:integration-codex')!.querySelector('button'), 'codex 還是落後，沒被動到')
  assert.ok(useStore.getState().notices.some((n) => n.kind === 'info' && /claude integration 已更新/.test(n.text)))
})

it('表頭：每一列都是未安裝或查不到時，不說「都是最新」（#1813）', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({ localTools: {} as ToolMap, localHerdr: undefined, upstreamUpdates: {} })
  await mount(<UpdateCheckPanel />)
  await until(() => row('local:herdr-cli') !== null, 'herdr 列出現')
  const head = document.querySelector('.update-host-count')?.textContent ?? ''
  assert.doesNotMatch(head, /都是最新/, `表頭 ${head}`)
  const ids = ['local:herdr-cli', 'local:herdr-server', 'local:cli-claude', 'local:cli-codex', 'local:cli-grok', 'local:cli-agy']
  const statuses = ids.map((id) => row(id)?.querySelector('.update-badge')?.textContent)
  assert.ok(statuses.every((t) => t === '未安裝' || t === '查不到'), `六列狀態 ${statuses}`)
  assert.match(head, /項未安裝|項查不到/)
})

it('表頭：工具清單還沒讀到（localTools undefined）時不是「未安裝」（#1813）', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({ localTools: undefined, localHerdr: undefined, upstreamUpdates: {} })
  await mount(<UpdateCheckPanel />)
  await until(() => row('local:cli-claude') !== null, 'CLI 列出現')
  const head = document.querySelector('.update-host-count')?.textContent ?? ''
  assert.doesNotMatch(head, /都是最新/, `表頭 ${head}`)
  assert.match(head, /查不到/)
  // integration 的未安裝來自 mock（那是真的沒裝）；這裡只看 herdr 與四家 CLI 這六列。
  const ids = ['local:herdr-cli', 'local:herdr-server', 'local:cli-claude', 'local:cli-codex', 'local:cli-grok', 'local:cli-agy']
  const statuses = ids.map((id) => row(id)?.querySelector('.update-badge')?.textContent)
  assert.ok(statuses.every((t) => t === '查不到'), `六列狀態 ${statuses}`)
})

it('沒裝的 grok／agy：已登入才有「安裝…」，確認框寫明會改 hook 設定檔，確認後才送 POST；沒登入沒有鈕；codex 沒裝照舊沒有鈕（#1871）', async () => {
  const requests = await open()
  const t = (installed: boolean, logged_in: boolean | null) => ({ installed, path: installed ? '/bin/x' : null, version: installed ? '1.0' : null, logged_in })
  await act(() => useStore.setState({ localTools: { claude: t(true, true), codex: t(true, true), grok: t(true, false), agy: t(true, true) } as unknown as ToolMap }))
  const post = () => requests.filter((r) => r.method === 'POST' && /herdr-integration$/.test(r.path))
  assert.equal(row('local:integration-grok')!.querySelector('button'), null, 'grok 沒登入不給鈕')
  assert.match(text('local:integration-grok'), /登入後/)
  const btn = row('local:integration-antigravity-cli')!.querySelector('button')
  assert.ok(btn, 'agy 已登入有鈕')
  assert.match(btn!.textContent ?? '', /安裝…/)
  await click(btn!)
  assert.equal(post().length, 0, '還沒確認，不能送')
  const dialog = document.querySelector('[role="alertdialog"]')!
  assert.match(dialog.textContent ?? '', /~\/\.gemini/, '確認框寫明會改 hook 設定檔')
  await click([...dialog.querySelectorAll('button')].find((b) => b.textContent === '安裝')!)
  await until(() => post().length === 1, '送了 POST')
  assert.deepEqual(post()[0].body, { name: 'antigravity-cli' })
  await until(() => /最新/.test(text('local:integration-antigravity-cli')), '那列變最新')
  assert.ok(useStore.getState().notices.some((n) => n.kind === 'info' && /antigravity-cli integration 已安裝/.test(n.text)))
})

it('裝完重讀失敗（200＋state:null＋reread_error）：toast 寫「裝完重讀失敗：原因」，不是「還不是最新」（#1871）', async () => {
  await open()
  const t = (installed: boolean, logged_in: boolean | null) => ({ installed, path: installed ? '/bin/x' : null, version: installed ? '1.0' : null, logged_in })
  await act(() => useStore.setState({ localTools: { claude: t(true, true), codex: t(true, true), grok: t(true, true), agy: t(true, true) } as unknown as ToolMap }))
  const real = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    if (init?.method === 'POST' && /herdr-integration$/.test(String(input))) {
      return new Response(JSON.stringify({ host: 'local', name: 'grok', state: null, integration: null, reread_error: 'ssh 逾時', output: 'ok' }), { status: 200 })
    }
    return real(input as never, init as never)
  }) as unknown as typeof fetch
  try {
    await click(row('local:integration-grok')!.querySelector('button')!)
    const dialog = document.querySelector('[role="alertdialog"]')!
    await click([...dialog.querySelectorAll('button')].find((b) => b.textContent === '安裝')!)
    await until(() => useStore.getState().notices.some((n) => n.kind === 'error'), '跳錯誤 toast')
    const n = useStore.getState().notices.find((x) => x.kind === 'error')!
    assert.match(n.text, /grok integration 裝完重讀失敗：ssh 逾時/)
    assert.doesNotMatch(n.text, /還不是最新/)
  } finally {
    globalThis.fetch = real
  }
})

it('agy 落後官方 manifest（#1908）：「更新」先問、確認後才送 POST /hosts/local/agy/install；確認框寫明不跑 install.sh／agy update；已是最新沒有鈕', async () => {
  const requests = await open()
  const t = (version: string) => ({ installed: true, path: '/bin/agy', version, logged_in: true })
  const agy = (installed: string, behind: boolean) => ({ kind: 'agy', latest: '1.4.5', target: '1.4.5', hasUpdate: behind, hosts: [{ host: 'local', installedVersion: installed, error: null, behind }], notify: null, text: null })
  await act(() => useStore.setState({ localTools: { claude: t('2.1.300'), codex: t('0.157.0'), grok: t('1.0'), agy: t('1.4.5') } as unknown as ToolMap, upstreamUpdates: { ...useStore.getState().upstreamUpdates, agy: agy('1.4.5', false) } }))
  assert.match(text('local:cli-agy'), /1\.4\.5.*最新/)
  assert.equal(row('local:cli-agy')!.querySelector('button'), null, '已是最新沒有鈕')

  await act(() => useStore.setState({ localTools: { claude: t('2.1.300'), codex: t('0.157.0'), grok: t('1.0'), agy: t('1.4.2') } as unknown as ToolMap, upstreamUpdates: { ...useStore.getState().upstreamUpdates, agy: agy('1.4.2', true) } }))
  assert.match(text('local:cli-agy'), /1\.4\.2.*1\.4\.5.*可更新/)
  const post = () => requests.filter((r) => r.method === 'POST' && /\/agy\/install$/.test(r.path))
  await click(row('local:cli-agy')!.querySelector('button')!)
  assert.equal(post().length, 0, '還沒確認，不能送')
  const dialog = document.querySelector('[role="alertdialog"]')!
  assert.match(dialog.textContent ?? '', /sha512.*不跑官方 install\.sh.*agy update/)
  await click([...dialog.querySelectorAll('button')].find((b) => b.textContent === '更新')!)
  await until(() => post().length === 1, '送了 POST')
  assert.match(post()[0].path, /\/hosts\/local\/agy\/install$/)
})
