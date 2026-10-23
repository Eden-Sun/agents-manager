/** 環境設定 → Claude API：沒 key 不給開、key 只送不收、用途各自開關與模型、帳本顯示累計 token（issue #1919）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { ClaudeApiPanel } from './ClaudeApiPanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const uses = (enabled: boolean) => [
  { id: 'changelog_review', label: 'AGM 解析 changelog', enabled, model: 'claude-sonnet-5-5', effort: 'medium', max_per_hour: 30, default_model: 'claude-sonnet-5-5', default_effort: 'medium', default_max_per_hour: 30, legacy_section: null },
  { id: 'changelog_translate', label: '更新框翻譯', enabled: false, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 12, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 12, legacy_section: 'changelog_translate' },
]
const settings = (over: Record<string, unknown>) => ({
  enabled: false,
  key_present: false,
  key_error: 'key file unreadable',
  models: ['claude-haiku-5-5', 'claude-sonnet-5-5'],
  efforts: ['low', 'medium', 'high', 'xhigh', 'max'],
  uses: uses(false),
  ...over,
})
const ledger = {
  totals: { calls: 3, ok: 2, errors: 1, input_tokens: 9120, output_tokens: 1480 },
  by_use: {},
  rows: [{ at: '2026-10-23T08:12:03.120Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 4210, ok: true, error_kind: null, input_tokens: 3480, output_tokens: 610, stop_reason: 'end_turn' }],
}

function stub(state: Record<string, unknown>, puts: unknown[]) {
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/claude-api/ledger') return new Response(JSON.stringify(ledger), { status: 200 })
    if (path === '/claude-api/settings' && init?.method === 'PUT') {
      puts.push(JSON.parse(String(init.body)))
      return new Response(JSON.stringify(settings({ enabled: true, key_present: true, key_error: null, uses: uses(true) })), { status: 200 })
    }
    if (path === '/claude-api/settings') return new Response(JSON.stringify(state), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
}

test('沒有 key 時總開關與各用途都不能開，帳本顯示累計 token', async () => {
  stub(settings({}), [])
  const root = await mount(<ClaudeApiPanel />)
  await settle()
  const boxes = [...root.querySelectorAll<HTMLInputElement>('input[type=checkbox]')]
  assert.ok(boxes.length >= 3)
  assert.ok(boxes.every((b) => b.disabled && !b.checked), '沒 key 全部不能勾')
  assert.match(root.textContent ?? '', /未設定（key file unreadable）/)
  assert.match(root.querySelector('[data-testid=claude-api-totals]')?.textContent ?? '', /共 3 次.*輸入 9,120.*輸出 1,480/)
  assert.match(root.textContent ?? '', /AGM 解析 changelog/)
})

test('貼 key 後可開；存檔只送 token、改過的用途，key 輸入框清空', async () => {
  const puts: unknown[] = []
  stub(settings({}), puts)
  const root = await mount(<ClaudeApiPanel />)
  await settle()
  const setValue = (el: HTMLInputElement | HTMLSelectElement, v: string) => {
    const proto = el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype
    Object.getOwnPropertyDescriptor(proto, 'value')?.set?.call(el, v)
    el.dispatchEvent(new Event(el instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }))
  }
  const key = root.querySelector<HTMLInputElement>('input[type=password]')!
  setValue(key, 'sk-ant-test')
  await settle()
  const master = root.querySelector<HTMLInputElement>('.claude-api-panel > .claude-api-switch input')!
  assert.equal(master.disabled, false, '貼了 key 就能開')
  master.click()
  await settle()
  const review = root.querySelector<HTMLFieldSetElement>('fieldset[data-use=changelog_review]')!
  review.querySelector<HTMLInputElement>('input[type=checkbox]')!.click()
  await settle()
  setValue(review.querySelector<HTMLSelectElement>('select')!, 'claude-haiku-5-5')
  await settle()
  root.querySelector<HTMLFormElement>('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
  await settle()
  assert.deepEqual(puts, [{ enabled: true, uses: { changelog_review: { enabled: true, model: 'claude-haiku-5-5' } }, token: 'sk-ant-test' }])
  assert.equal(root.querySelector<HTMLInputElement>('input[type=password]')!.value, '')
  assert.match(root.textContent ?? '', /已儲存/)
})

test('ClaudeApiPanel: 子分頁切換「設定｜帳本」，預設設定，點帳本切換並渲染用量表格', async () => {
  stub(settings({}), [])
  const root = await mount(<ClaudeApiPanel />)
  await settle()

  // 驗證 subtabs 結構
  const subtabs = root.querySelector('.env-subtabs[role="tablist"]')
  assert.ok(subtabs, 'subtabs tablist should exist')
  assert.equal(subtabs.getAttribute('aria-label'), 'Claude API 分類')

  const tabs = subtabs.querySelectorAll<HTMLButtonElement>('button[role="tab"]')
  assert.equal(tabs.length, 2, '應有 2 個子分頁')
  assert.equal(tabs[0].textContent, '設定')
  assert.equal(tabs[1].textContent, '帳本')
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true', '預設選中「設定」')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'false')

  // 設定 panel 可見，帳本 panel hidden
  const formPanel = root.querySelector('#api-form')
  const bookPanel = root.querySelector('#api-book')
  assert.ok(formPanel, 'api-form panel exists')
  assert.ok(bookPanel, 'api-book panel exists')
  assert.equal(formPanel.hasAttribute('hidden'), false, 'formPanel should be visible')
  assert.equal(bookPanel.hasAttribute('hidden'), true, 'bookPanel should be hidden')

  // 點擊「帳本」分頁
  tabs[1].click()
  await settle()

  assert.equal(tabs[0].getAttribute('aria-selected'), 'false')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.equal(formPanel.hasAttribute('hidden'), true, 'formPanel should be hidden')
  assert.equal(bookPanel.hasAttribute('hidden'), false, 'bookPanel should be visible')

  // 驗證帳本內容
  assert.match(bookPanel.textContent ?? '', /用量帳本/)
  assert.match(bookPanel.textContent ?? '', /AGM 解析 changelog/)
  const rows = bookPanel.querySelectorAll('tbody tr')
  assert.equal(rows.length, 1)

  // 切回「設定」分頁
  tabs[0].click()
  await settle()
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true')
  assert.equal(formPanel.hasAttribute('hidden'), false)
  assert.equal(bookPanel.hasAttribute('hidden'), true)
})
