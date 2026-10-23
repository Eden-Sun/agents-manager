/**
 * 首次啟動的環境準備清單（issue #1931）：沒裝／沒登入任何 CLI 時主畫面出現清單，每列有安裝／登入鈕；
 * claude／codex／grok 的安裝不帶 `via_bot_id`（daemon 開臨時 shell），agy 走既有 agy/install 的確認框；
 * 新增 Bot 的快速鈕裡未登入的項目可點、點了開登入流程。真的掛元件進 happy-dom，後端是 mock。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { dispatchFrameForTest, resetStoreForTest, useStore } from '../store/store'
import type { ToolMap } from '../api/types'
import { OnboardingBanner, OnboardingChecklist } from './OnboardingChecklist'
import { QuickAddBots } from './QuickAddBots'

virtualMockTime()
async function closeShells() {
  const api = await import('../api')
  for (const sh of await api.fetchHostShells('local')) await api.closeHostShell('local', sh.pane_id, true)
}
afterEach(async () => {
  await unmountAll()
  await closeShells()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const row = (kind: string) => document.querySelector(`.onb-row[data-kind="${kind}"]`)!
const none = { installed: false, path: null, version: null, logged_in: null }
const fresh = (over: Partial<ToolMap> = {}): ToolMap => ({ claude: none, codex: none, grok: none, agy: none, ...over })

/** 共用的 mock 是行程單例：這支把它的本機 tools 也換成同一份，之後 `refreshTools` 讀回來才對得上。 */
type MockInternals = { localTools: ToolMap; localIdentityStatus: Record<string, unknown> }
const internals = sharedMock as unknown as MockInternals
const originalMock = { tools: { ...internals.localTools }, identities: internals.localIdentityStatus }
after(() => {
  internals.localTools = originalMock.tools
  internals.localIdentityStatus = originalMock.identities
})

async function setLocal(tools: ToolMap, projects = false) {
  internals.localTools = { ...tools }
  internals.localIdentityStatus = {}
  await act(async () => {
    useStore.setState((s) => ({ localTools: tools, localIdentityStatus: {}, connected: true, projects: projects ? s.projects : [] }))
  })
}

it('一個 CLI 都沒裝：四列都是未安裝＋安裝鈕，下一步引導先裝 CLI', async () => {
  await setLocal(fresh())
  await mount(<OnboardingBanner compact={false} />)
  for (const k of ['claude', 'codex', 'grok', 'agy']) assert.equal(row(k).getAttribute('data-state'), 'missing', k)
  assert.equal(row('herdr').getAttribute('data-state'), 'ok')
  assert.match(document.body.textContent ?? '', /還沒有可用的 agent CLI/)
  assert.ok(btn(row('claude'), '安裝'))
  assert.equal(btn(document.body, '新增 Project')?.disabled, true, 'CLI 還沒就緒前不能新增 Project')
})

it('claude 安裝：不帶 via_bot_id，daemon 開 shell；裝好後那列翻成已安裝・未登入並出現登入鈕', async () => {
  await setLocal(fresh())
  const requests = mockApi(sharedMock)
  await mount(<OnboardingBanner compact={false} />)
  await click(btn(row('claude'), '安裝')!)
  const post = requests.find((r) => r.method === 'POST' && /\/hosts\/local\/tools\/install$/.test(r.path))
  assert.ok(post, '打了 tools/install')
  assert.deepEqual(post.body, { kind: 'claude' }, '不帶 via_bot_id')
  assert.ok(useStore.getState().shellView, '畫面切到安裝的 shell pane')
  // 測試沒有 socket：daemon 的 `host_changed` 用重新偵測代替。
  await until(async () => {
    await useStore.getState().refreshTools('local')
    return row('claude').getAttribute('data-state') === 'out'
  }, '裝好後翻成未登入')
  assert.ok(btn(row('claude'), '開 shell 登入'), '未登入有登入鈕')
})

it('問不到登入狀態（null）顯示「問不到」，不說未登入，也沒有登入鈕', async () => {
  await setLocal(fresh({ grok: { installed: true, path: '/bin/grok', version: '1.0.0', logged_in: null } }))
  await mount(<OnboardingChecklist />)
  assert.equal(row('grok').getAttribute('data-state'), 'unknown')
  assert.match(row('grok').textContent ?? '', /問不到/)
  assert.doesNotMatch(row('grok').textContent ?? '', /未登入/)
  assert.equal(btn(row('grok'), '登入'), undefined)
})

it('agy 的安裝沿用確認框（不直接打 agy/install）', async () => {
  await setLocal(fresh())
  const requests = mockApi(sharedMock)
  await mount(<OnboardingChecklist />)
  await click(btn(row('agy'), '安裝 agy')!)
  await until(() => (document.body.textContent ?? '').includes('尚未安裝 agy，要自動安裝嗎？'), '確認框出現')
  assert.equal(requests.filter((r) => /agy\/install$/.test(r.path)).length, 0)
})

it('有 Project 且有就緒的 CLI：主畫面自動收起；環境設定用的清單照樣畫得出來', async () => {
  const ready = { installed: true, path: '/bin/claude', version: '2.1.0', logged_in: true }
  await setLocal(fresh({ claude: ready }))
  await act(async () => {
    useStore.setState({ projects: [{ id: 'p1', label: 'p', path: '/tmp', host: 'local' } as never] })
  })
  await mount(<OnboardingBanner compact={false} />)
  assert.equal(document.querySelector('.onb-banner'), null)
  await unmountAll()
  await mount(<OnboardingChecklist />)
  assert.equal(row('claude').getAttribute('data-state'), 'ok')
  assert.match(document.body.textContent ?? '', /環境已就緒/)
})

it('新增 Bot 快速鈕：未登入的 kind 可點、點了開登入終端（不是新增 Bot）', async () => {
  await act(async () => {
    useStore.setState({
      localTools: fresh({ codex: { installed: true, path: '/bin/codex', version: '0.1.0', logged_in: false } }),
      projects: [{ id: 'p1', label: 'p', path: '/tmp', host: 'local' } as never],
      bots: [],
    })
  })
  const requests = mockApi(sharedMock)
  await mount(<QuickAddBots projectId="p1" />)
  const chip = btn(document.body, 'codex')!
  assert.equal(chip.disabled, false, '未登入的 chip 不再 disabled')
  assert.match(chip.textContent ?? '', /未登入/)
  await click(chip)
  await until(() => requests.some((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path)), '開了登入 shell')
  assert.equal(requests.some((r) => /\/projects\/p1\/bots$/.test(r.path)), false, '沒有新增 Bot')
})

it('安裝中鈕維持「安裝中…」，tool_install_done 沒裝成：放開鈕、講逾時、打開留著的 pane', async () => {
  await setLocal(fresh())
  mockApi(sharedMock)
  await mount(<OnboardingBanner compact={false} />)
  await click(btn(row('codex'), '安裝')!)
  assert.match(row('codex').textContent ?? '', /安裝中…/)
  const pane = useStore.getState().shellView!.paneId
  await act(async () => {
    useStore.setState({ shellView: null, notices: [] })
    dispatchFrameForTest({ type: 'tool_install_done', data: { host: 'local', kind: 'codex', installed: false, pane_id: pane, pane_kept: true, timed_out: true } })
  })
  await until(() => useStore.getState().shellView?.paneId === pane, '打開安裝的 pane 讓使用者看錯誤')
  assert.match(row('codex').textContent ?? '', /安裝/)
  assert.doesNotMatch(row('codex').textContent ?? '', /安裝中…/)
  assert.ok(useStore.getState().notices.some((n) => /逾時/.test(n.text)), '通知講清楚是逾時')
})

it('tool_install_done 裝成功：放開鈕、不開 pane', async () => {
  await setLocal(fresh())
  mockApi(sharedMock)
  await mount(<OnboardingBanner compact={false} />)
  await click(btn(row('grok'), '安裝')!)
  await act(async () => {
    useStore.setState({ shellView: null })
    dispatchFrameForTest({ type: 'tool_install_done', data: { host: 'local', kind: 'grok', installed: true, pane_id: 'w9:s9', pane_kept: false, timed_out: false } })
  })
  assert.equal(useStore.getState().shellView, null)
  assert.doesNotMatch(row('grok').textContent ?? '', /安裝中…/)
})
