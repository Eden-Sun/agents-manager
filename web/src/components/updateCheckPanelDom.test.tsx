/**
 * 環境設定「更新檢查」（UpdateCheckPanel，#1772）：每台主機一張表，herdr／各 agent CLI／herdr integration 都在。
 * 真的掛進 happy-dom，後端是 mock。integration 的更新只在按下「更新…」並確認後才送 POST；沒裝的、已是最新的沒有按鈕。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
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
