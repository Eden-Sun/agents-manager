/**
 * 手機（≤640px）「Git / 專案資訊」彈窗（真的掛 `ChatPanel` 進 happy-dom，後端是 `MockTransport`）：
 * - #929 → 2026-11-03：「bot 給你的檔案」不在彈窗裡，改在對話下方的收合列（`PhoneOutboxTray`）。
 * - #930：帳號警告在手機不被藏，彈窗裡看得到、標題列的 Git 鈕帶 `warn`。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, keydown, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
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
    context_unknown: false,
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

test('桌機：GitHub 收合，狀態列保留，按鈕可展開並關閉資訊', async () => {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() => useStore.setState({ selectedBotId: id, runs: { ...useStore.getState().runs, [id]: { ...run, status: statusWith(null) } } }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    const btn = document.querySelector<HTMLButtonElement>('.desktop-git-info')!
    assert.ok(btn)
    assert.equal(btn.getAttribute('aria-expanded'), 'false')
    assert.ok(document.querySelector('.context-bar > .statusline-bar'))
    assert.equal(document.querySelector('.issues-bar'), null)
    await click(btn)
    await until(() => document.querySelector('.modal .issues-bar') !== null, '展開 GitHub')
    assert.equal(btn.getAttribute('aria-expanded'), 'true')
    assert.ok(document.querySelector('.context-bar > .statusline-bar'))
    await click(document.querySelector<HTMLButtonElement>('.modal button[aria-label="關閉"]')!)
    assert.equal(btn.getAttribute('aria-expanded'), 'false')
    assert.equal(document.querySelector('.modal .issues-bar'), null)
  } finally {
    window.matchMedia = original
  }
})

test('手機：Git 彈窗不再放「bot 給你的檔案」（2026-11-03 改到對話下方的收合列）', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(gitBtn().getAttribute('aria-label'), 'Context 與專案')
    await click(gitBtn())
    await until(() => document.querySelector('.mobile-context-panel') !== null, 'popup 打開')
    assert.equal(document.querySelector('.mobile-context-panel .outbox-files'), null, '彈窗裡沒有 outbox')
    assert.equal(document.querySelector('.modal'), null, '不開全螢幕 modal')
    await until(() => (document.querySelector('.phone-outbox-bar')?.textContent ?? '').includes('bot 給的檔案'), '輸入框上方的收合列')
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
        runs: { ...useStore.getState().runs, [id]: { ...run, status: statusWith('身份 cc9 在這台沒登入，用的是預設帳號') } },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.ok(gitBtn().classList.contains('warn'), 'Git 鈕要有警告點')
    assert.match(gitBtn().getAttribute('aria-label') ?? '', /帳號警告/)
    await click(gitBtn())
    await until(() => document.querySelector('.mobile-context-panel .sl-warn') !== null, '彈窗裡有帳號警告')
    assert.match(document.querySelector('.mobile-context-panel .sl-warn')!.textContent ?? '', /未登入/)
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
    // 只有百分比、沒有 token：圖示在上、百分比在下（不捏一個假的 k）。
    assert.equal(gitBtn().getAttribute('aria-label'), 'Context 與專案 · context 12%')
    assert.equal(gitBtn().querySelector('.mobile-ctx')?.textContent, '12%')
    assert.equal(gitBtn().querySelector('.mobile-ctx-k'), null)
    assert.ok(gitBtn().querySelector('svg'))
    assert.ok(gitBtn().classList.contains('has-ctx'))
    assert.equal(gitBtn().classList.contains('has-both'), false)
  } finally {
    restore()
  }
})

test('手機：有 token 時 Git 鈕上排已用、下排百分比', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() =>
      useStore.setState({
        selectedBotId: id,
        runs: {
          ...useStore.getState().runs,
          [id]: {
            ...run,
            status: { ...statusWith(null), context_used_pct: 18, context_used_tokens: 91_000, context_size: 500_000 },
          },
        },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(gitBtn().getAttribute('aria-label'), 'Context 與專案 · context 91k · 18%')
    assert.equal(gitBtn().querySelector('.mobile-ctx-k')?.textContent, '91k')
    assert.equal(gitBtn().querySelector('.mobile-ctx-pct')?.textContent, '18%')
    assert.equal(gitBtn().querySelector('svg'), null, '兩行數字不再加 Git 圖示')
    assert.ok(gitBtn().classList.contains('has-both'))
  } finally {
    restore()
  }
})

test('手機：claude 沒有 statusLine 讀數時，退回 daemon 的 prompt_cache（297k · 30%）', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() =>
      useStore.setState({
        selectedBotId: id,
        runs: {
          ...useStore.getState().runs,
          [id]: { ...run, status: null, prompt_cache: { ...(run.prompt_cache ?? {}), context_used_tokens: 296_758, context_size: 1_000_000 } as never },
        },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(gitBtn().querySelector('.mobile-ctx-k')?.textContent, '297k')
    assert.equal(gitBtn().querySelector('.mobile-ctx-pct')?.textContent, '30%')
  } finally {
    restore()
  }
})

test('手機：剛 resume 的 context 仍是破折號', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() =>
      useStore.setState({
        selectedBotId: id,
        runs: {
          ...useStore.getState().runs,
          [id]: {
            ...run,
            status: { ...statusWith(null), context_used_pct: null, context_used_tokens: null, context_size: null, context_unknown: true },
          },
        },
      }),
    )
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(gitBtn().querySelector('.mobile-ctx')?.textContent, '—')
    assert.ok(gitBtn().querySelector('svg'))
    assert.equal(gitBtn().classList.contains('has-both'), false)
    assert.equal(gitBtn().getAttribute('aria-label'), 'Context 與專案')
  } finally {
    restore()
  }
})

test('CSS：手機只收帳號 email，不收帳號警告', () => {
  const chat = readFileSync(new URL('./chatPanel.css', import.meta.url), 'utf8')
  assert.match(chat, /\.sl-item\.sl-account:not\(\.sl-warn\)\s*\{\s*display:\s*none/, '收掉帳號欄的規則要排除 sl-warn')
  assert.doesNotMatch(chat, /\.sl-item\.sl-account\s*\{\s*display:\s*none/, '不能有無條件收掉整個帳號欄的規則')
  assert.match(chat, /\.context-bar \.sl-item\.sl-account:not\(\.sl-warn\) \+ \.sl-item/, '分隔線規則同步')
  assert.match(chat, /\.mobile-git-info\.warn::after/)
})

test('手機：小 popup 只留 context、快取、Git 與 Issues，焦點在面板', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    mockApi({ upload: (...args) => mock.upload(...args), request: (method, path, body) => path.endsWith('/git')
      ? Promise.resolve({ git: true, branch: 'feature/context-panel', upstream: 'origin/main', insertions: 12, deletions: 3, untracked: 1, changed: 2, ahead: 0, behind: 1 })
      : mock.request(method, path, body) })
    const run = useStore.getState().runs[id]!
    await act(() => useStore.setState({ selectedBotId: id, turns: { ...useStore.getState().turns, [id]: {} }, runs: { ...useStore.getState().runs, [id]: {
      ...run, agent_status: 'idle', cache_kept_warm_at: null, prompt_cache: null, last_api_at: new Date().toISOString(), cache_ttl_secs: 3600,
      status: { ...statusWith(null), context_used_tokens: 67_000, context_size: 1_000_000, context_used_pct: 7,
        account_email: 'long-account@example.com', effort: 'high', five_hour_pct: 15, seven_day_pct: 27, cost_usd: 1.23 },
    } } }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await click(gitBtn())
    const panel = document.querySelector('.mobile-context-panel')!
    assert.equal(panel.querySelector('section, h3, dl'), null, '沒有卡片、區塊標題或帳號額度清單')
    assert.equal(panel.querySelector('.mobile-context-reading')?.textContent, '67k / 1M · 7%')
    assert.equal(panel.querySelector('progress')?.getAttribute('value'), '7')
    assert.match(panel.querySelector('.mobile-context-cache')?.textContent ?? '', /熱.*分/)
    assert.equal(panel.querySelectorAll('.compact-btn').length, 1)
    assert.doesNotMatch(panel.textContent ?? '', /long-account@example.com|剩餘|\$1.23|Opus/)
    assert.equal(panel.querySelector('.issues-pop'), null)
    assert.equal(document.activeElement, panel, '不把焦點給 Issues')
    await until(() => panel.querySelector('.git-branch') !== null, 'Git 分支')
    assert.equal(panel.querySelector('.git-actions'), null, '手機只有 Git 狀態，不渲染動作')
    assert.equal(panel.querySelector('.git-bar button'), null)
    assert.match(panel.querySelector('.git-bar')?.textContent ?? '', /\+12−3\?1↓1/)
    await click(panel.querySelector<HTMLButtonElement>('.issues-btn')!)
    await until(() => panel.querySelector('.issues-pop') !== null, '點開 Issues 才顯示清單')
  } finally { restore() }
})

test('手機：resume 不顯示殘留 token，快取 Cold 完整呈現', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const run = useStore.getState().runs[id]!
    await act(() => useStore.setState({ selectedBotId: id, turns: { ...useStore.getState().turns, [id]: {} }, runs: { ...useStore.getState().runs, [id]: {
      ...run, agent_status: 'idle', cache_kept_warm_at: null, prompt_cache: null, last_api_at: '2020-01-01T00:00:00Z', cache_ttl_secs: 3600,
      status: { ...statusWith(null), context_unknown: true, context_used_tokens: 67_000, context_size: 1_000_000 },
    } } }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await click(gitBtn())
    const panel = document.querySelector('.mobile-context-panel')!
    assert.equal(panel.querySelector('.mobile-context-reading')?.textContent, '—')
    assert.equal(panel.querySelector('progress')?.getAttribute('aria-valuetext'), '—')
    assert.match(panel.querySelector('.mobile-context-cache')?.textContent ?? '', /已冷/)
  } finally { restore() }
})

test('手機：popup 可再次點擊、點外面、Esc 關閉，固定在入口下方', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    const button = gitBtn()
    button.getBoundingClientRect = () => ({ left: 10, bottom: 120, width: 40, height: 40, top: 80, right: 50, x: 10, y: 80, toJSON() {} })
    await click(button)
    const popup = document.querySelector<HTMLElement>('.mobile-context-panel')!
    assert.equal(popup.style.top, '126px')
    assert.equal(document.querySelector('.modal-backdrop'), null)
    assert.equal(popup.getAttribute('aria-modal'), null)
    await click(button)
    assert.equal(document.querySelector('.mobile-context-panel'), null)
    await click(button)
    await act(() => document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true })))
    assert.equal(document.querySelector('.mobile-context-panel'), null)
    await click(button)
    await keydown(document.querySelector('.mobile-context-panel')!, 'Escape')
    assert.equal(document.querySelector('.mobile-context-panel'), null)
    assert.equal(document.activeElement, button)
  } finally { restore() }
})

test('手機：簡單 popup 英文完整、Hot 有剩餘分鐘，不放模型額度花費', async () => {
  const { setLangOverride } = await import('../lib/i18n/lang')
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    const state = useStore.getState()
    const run = state.runs[id]!
    await act(() => useStore.setState({ selectedBotId: id,
      runs: { ...state.runs, [id]: { ...run, agent_status: 'idle', prompt_cache: null, cache_kept_warm_at: null, last_api_at: new Date().toISOString(), cache_ttl_secs: 720,
        status: { ...statusWith(null), context_used_pct: 7, context_used_tokens: 67_000, context_size: 1_000_000, model_name: 'hidden-model', cost_usd: 1.23 } } },
      turns: { ...state.turns, [id]: {} },
    }))
    await act(() => setLangOverride('en'))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await click(gitBtn())
    const popup = document.querySelector('.mobile-context-panel')!
    assert.match(popup.textContent ?? '', /Hot · 12 min left/)
    assert.doesNotMatch(popup.textContent ?? '', /hidden-model|Cost|Account|remaining|\$1.23|[\u3400-\u9fff]/)
  } finally {
    await act(() => setLangOverride(null))
    restore()
  }
})
