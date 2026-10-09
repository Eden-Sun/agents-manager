/**
 * 手機（≤640px）「Git / 專案資訊 / 檔案」彈窗（真的掛 `ChatPanel` 進 happy-dom，後端是 `MockTransport`）：
 * - #929：彈窗裡畫「bot 給你的檔案」（手機沒有檔案暫存托盤，outbox 沒有別的入口）。
 * - #930：帳號警告在手機不被藏，彈窗裡看得到、標題列的 Git 鈕帶 `warn`。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import type { StatusInfo } from '../api/types'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const gitBtn = () => document.querySelector<HTMLButtonElement>('.mobile-git-info')!

/** 手機寬度：`matchMedia` 對 640px 斷點回 true。 */
function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

async function running(name: string): Promise<string> {
  mockApi(mock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === name)!
  assert.ok(bot, `mock 要有 ${name}`)
  if (useStore.getState().runs[bot.id]?.state !== 'running') {
    await mock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running'
    }, `${name} running`)
  }
  return bot.id
}

function statusWith(account_warning: string | null): StatusInfo {
  return {
    account_email: null,
    account_warning,
    model_name: 'Opus',
    model_id: null,
    effort: null,
    thinking: false,
    fast_mode: false,
    context_used_pct: 12,
    context_used_tokens: null,
    context_size: null,
    five_hour_pct: null,
    five_hour_resets_at: null,
    seven_day_pct: null,
    seven_day_resets_at: null,
    cost_usd: null,
    cwd: null,
    version: null,
    session_name: null,
  }
}

test('手機：Git 鈕打開的彈窗裡有「bot 給你的檔案」，列出 outbox 的檔名（#929）', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(document.querySelector('.outbox-files'), null, '彈窗沒開就沒有')
    assert.equal(gitBtn().getAttribute('aria-label'), 'Git / 專案資訊 / 檔案', '入口的名字要提到檔案')
    await click(gitBtn())
    await until(() => document.querySelector('.modal .outbox-files') !== null, '彈窗裡有 outbox 區塊')
    await until(() => (document.querySelector('.modal .outbox-files')?.textContent ?? '').includes('tracking.tsv'), '列出 mock 的檔名')
    assert.ok(document.querySelector('.modal')?.textContent?.includes('Git / 專案資訊 / 檔案'), '彈窗標題')
  } finally {
    restore()
  }
})

test('手機：帳號警告不被藏——彈窗狀態列有「未登入」、Git 鈕帶 warn 與（帳號警告）（#930）', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() =>
      useStore.setState({
        selectedBotId: id,
        runs: { ...useStore.getState().runs, [id]: { ...run, status: statusWith('身分 cc9 在這台沒登入，用的是預設帳號') } },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.ok(gitBtn().classList.contains('warn'), 'Git 鈕要有警告點')
    assert.match(gitBtn().getAttribute('aria-label') ?? '', /帳號警告/)
    await click(gitBtn())
    await until(() => document.querySelector('.modal .sl-warn') !== null, '彈窗裡有帳號警告')
    assert.match(document.querySelector('.modal .sl-warn')!.textContent ?? '', /未登入/)
  } finally {
    restore()
  }
})

test('手機：沒有帳號警告時 Git 鈕不帶 warn', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() =>
      useStore.setState({
        selectedBotId: id,
        runs: { ...useStore.getState().runs, [id]: { ...run, status: statusWith(null) } },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(gitBtn().classList.contains('warn'), false)
    assert.equal(gitBtn().getAttribute('aria-label'), 'Git / 專案資訊 / 檔案')
  } finally {
    restore()
  }
})

test('CSS：手機只收帳號 email，不收帳號警告；outbox 在彈窗裡不套托盤的高度上限', () => {
  const chat = readFileSync(new URL('./chatPanel.css', import.meta.url), 'utf8')
  assert.match(chat, /\.sl-item\.sl-account:not\(\.sl-warn\)\s*\{\s*display:\s*none/, '收掉帳號欄的規則要排除 sl-warn')
  assert.doesNotMatch(chat, /\.sl-item\.sl-account\s*\{\s*display:\s*none/, '不能有無條件收掉整個帳號欄的規則')
  assert.match(chat, /\.context-bar \.sl-item\.sl-account:not\(\.sl-warn\) \+ \.sl-item/, '分隔線規則同步')
  assert.match(chat, /\.mobile-git-info\.warn::after/)
  const outbox = readFileSync(new URL('./outboxFiles.css', import.meta.url), 'utf8')
  assert.match(outbox, /\.modal \.outbox-files\s*\{\s*max-height:\s*none/)
})
