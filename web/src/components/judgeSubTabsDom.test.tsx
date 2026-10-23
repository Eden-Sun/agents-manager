import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { JudgePanel } from './JudgePanel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const SETTINGS = { enabled: false, projects: [], model: 'jev-1.13.0', key_present: false, key_error: null }

function stubApi(routes: Record<string, { status: number; body?: unknown }>) {
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    const key = path.split('?')[0]!
    const r = routes[key] ?? { status: 404, body: {} }
    return new Response(JSON.stringify(r.body ?? {}), { status: r.status })
  }) as unknown as typeof fetch
}

test('JudgePanel: 分「設定｜帳本」兩個子分頁，預設停在設定；最近 20 筆預設收合在 details.env-fold', async () => {
  stubApi({
    '/judge/settings': { status: 200, body: SETTINGS },
    '/judge/shadow': {
      status: 200,
      body: {
        enabled: false,
        projects: [],
        model: 'jev-1.13.0',
        rows: [
          {
            id: 'r-1',
            bot_id: null,
            run_id: null,
            kind: null,
            at: new Date().toISOString(),
            regex_verdict: 'stuck_queued',
            jev_is_live_ui: 0.9,
            claims_verified: null,
            claims_required: null,
            ms: 200,
            error: null,
            matched_line: '',
          },
        ],
      },
    },
  })

  const root = await mount(<JudgePanel />)
  await settle()

  // 子分頁 tablist
  const tabs = root.querySelectorAll<HTMLButtonElement>('[role="tab"]')
  assert.equal(tabs.length, 2, '應有「設定」與「帳本」兩個子分頁')
  assert.equal(tabs[0].textContent?.trim(), '設定')
  assert.equal(tabs[1].textContent?.trim(), '帳本')
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true', '預設選中設定')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'false')

  // 面板顯示狀態
  const formPanel = root.querySelector('#jev-form')
  const bookPanel = root.querySelector('#jev-book')
  assert.ok(formPanel, '#jev-form 存在')
  assert.ok(bookPanel, '#jev-book 存在')
  assert.equal(formPanel.getAttribute('hidden'), null, '設定面板未隱藏')
  assert.notEqual(bookPanel.getAttribute('hidden'), null, '帳本面板已隱藏')

  // 切換至帳本
  await click(tabs[1])
  assert.equal(tabs[0].getAttribute('aria-selected'), 'false')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.notEqual(formPanel.getAttribute('hidden'), null, '設定面板已隱藏')
  assert.equal(bookPanel.getAttribute('hidden'), null, '帳本面板未隱藏')

  // 檢查「最近 20 筆」預設收起於 details.env-fold
  const fold = bookPanel.querySelector<HTMLDetailsElement>('details.env-fold')
  assert.ok(fold, '「最近 20 筆」包在 details.env-fold')
  assert.equal(fold.open, false, '預設為收起（open === false）')
  assert.equal(fold.querySelector('summary')?.textContent?.trim(), '最近 20 筆')
})
