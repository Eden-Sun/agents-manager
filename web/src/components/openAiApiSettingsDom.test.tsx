import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { OpenAiApiSettings } from './OpenAiApiSettings'
import { ClaudeApiPanel } from './ClaudeApiPanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const submit = (root: Element) => root.querySelector<HTMLFormElement>('form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))

const SENTINEL = 'sk-test-SENTINEL-XYZ'
const base = { enabled: false, key_file: '~/.config/agents-manager/openai-api-key', key_present: false, key_error: 'key file unreadable', api_bot: { total_cap_usd: null, total_cap_since: null }, spent_usd: 0.0123, spent_bots_usd: 0.01, spent_uses_usd: 0.0023 }

function stub(state: Record<string, unknown>, puts: unknown[], next?: Record<string, unknown>, urls: string[] = []) {
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    urls.push(String(input))
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/openai-api/settings' && init?.method === 'PUT') {
      puts.push(JSON.parse(String(init.body)))
      return new Response(JSON.stringify(next ?? state), { status: 200 })
    }
    if (path === '/openai-api/settings') return new Response(JSON.stringify(state), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
}

test('沒有 key：開關不能開，狀態寫原因；顯示累計花費與 key 路徑，沒有任何 key 內容', async () => {
  stub(base, [])
  const root = await mount(<OpenAiApiSettings />)
  await settle()
  const sw = root.querySelector<HTMLInputElement>('[data-testid=openai-api-enabled]')!
  assert.equal(sw.disabled, true)
  assert.equal(sw.checked, false)
  assert.match(root.querySelector('[data-testid=openai-api-key-status]')?.textContent ?? '', /未設定（key file unreadable）/)
  assert.equal(root.querySelector<HTMLInputElement>('[data-testid=openai-api-key-file]')!.value, '~/.config/agents-manager/openai-api-key')
  assert.match(root.querySelector('[data-testid=openai-api-spent]')?.textContent ?? '', /\$0\.012.*bot \$0\.010.*\$0\.002/)
  assert.equal(root.querySelector('input[type=password]'), null, '這一區沒有貼 key 的欄位')
  assert.ok(!(root.textContent ?? '').includes(SENTINEL))
})

test('有 key：打開開關並設上限存檔，PUT 只送改過的欄位；回來的狀態套用、起算時間顯示', async () => {
  const puts: unknown[] = []
  const saved = { ...base, enabled: true, key_present: true, key_error: null, api_bot: { total_cap_usd: 5, total_cap_since: '2026-11-18T10:00:00.000Z' } }
  stub({ ...base, key_present: true, key_error: null }, puts, saved)
  const root = await mount(<OpenAiApiSettings />)
  await settle()
  assert.match(root.querySelector('[data-testid=openai-api-key-status]')?.textContent ?? '', /已設定/)
  const save = root.querySelector<HTMLButtonElement>('[data-testid=openai-api-save]')!
  assert.equal(save.disabled, true, '沒改東西不能存')
  await click(root.querySelector('[data-testid=openai-api-enabled]')!)
  await typeInto(root.querySelector<HTMLInputElement>('[data-testid=openai-api-cap]')!, '5')
  assert.equal(save.disabled, false)
  submit(root)
  await until(() => puts.length === 1, 'PUT')
  assert.deepEqual(puts, [{ enabled: true, total_cap_usd: 5 }])
  await settle()
  assert.match(root.textContent ?? '', /已儲存/)
  assert.match(root.querySelector('[data-testid=openai-api-cap-since]')?.textContent ?? '', /2026-11-18 10:00:00/)
  assert.equal(root.querySelector<HTMLInputElement>('[data-testid=openai-api-cap]')!.value, '5')
})

test('清空上限＝PUT total_cap_usd:null；不合法的上限不能存', async () => {
  const puts: unknown[] = []
  stub({ ...base, enabled: true, key_present: true, key_error: null, api_bot: { total_cap_usd: 5, total_cap_since: '2026-11-18T10:00:00.000Z' } }, puts)
  const root = await mount(<OpenAiApiSettings />)
  await settle()
  const cap = root.querySelector<HTMLInputElement>('[data-testid=openai-api-cap]')!
  const save = root.querySelector<HTMLButtonElement>('[data-testid=openai-api-save]')!
  await typeInto(cap, '-3')
  assert.equal(save.disabled, true, '負數不能存')
  await typeInto(cap, '')
  assert.equal(save.disabled, false)
  submit(root)
  await until(() => puts.length === 1, 'PUT')
  assert.deepEqual(puts, [{ total_cap_usd: null }])
})

test('舊 daemon（端點 404）：顯示需要更新，不炸', async () => {
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  const root = await mount(<OpenAiApiSettings />)
  await settle()
  assert.match(root.querySelector('[data-testid=openai-api-unsupported]')?.textContent ?? '', /需要更新 daemon/)
})

test('帳本：供應商篩選帶 provider 參數；模型欄用短名（Luna）、完整 id 在 tooltip', async () => {
  const urls: string[] = []
  const settings = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [], groups: [] }
  const book = (rows: unknown[]) => ({ totals: { calls: rows.length, ok: rows.length, errors: 0, input_tokens: 1, output_tokens: 1 }, by_use: {}, rows })
  const row = (model: string, provider: string) => ({ at: '2026-11-18T08:00:00.000Z', use: 'message_translate', model, provider, ms: 100, ok: true, error_kind: null, input_tokens: 1, output_tokens: 1, stop_reason: null })
  globalThis.fetch = (async (input: string) => {
    urls.push(String(input))
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/claude-api/settings') return new Response(JSON.stringify(settings), { status: 200 })
    if (path === '/claude-api/ledger') {
      const only = String(input).includes('provider=openai')
      return new Response(JSON.stringify(book(only ? [row('gpt-6-luna', 'openai')] : [row('claude-haiku-5-5', 'anthropic'), row('gpt-6-luna', 'openai')])), { status: 200 })
    }
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  const root = await mount(<ClaudeApiPanel />)
  await settle()
  const cells = () => [...root.querySelectorAll<HTMLElement>('tbody td[data-label="模型"]')]
  assert.deepEqual(cells().map((c) => c.textContent?.trim()), ['Haiku', 'Luna OpenAI'])
  assert.deepEqual(cells().map((c) => c.title), ['claude-haiku-5-5', 'gpt-6-luna'])
  const sel = root.querySelector<HTMLSelectElement>('[data-testid=claude-api-ledger-provider]')!
  Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')?.set?.call(sel, 'openai')
  sel.dispatchEvent(new Event('change', { bubbles: true }))
  await until(() => urls.some((u) => u.includes('/claude-api/ledger') && u.includes('provider=openai')), '帶 provider=openai 重抓')
  await settle()
  assert.deepEqual(cells().map((c) => c.title), ['gpt-6-luna'])
})

test('OpenAI API 是和 Claude API 並列的獨立分頁；Claude 那一頁不再內嵌 OpenAI 設定區', async () => {
  const { EnvShell } = await import('./EnvShell')
  stub({ ...base, key_present: true, key_error: null }, [])
  const root = await mount(<EnvShell />)
  await settle()
  const tab = root.querySelector<HTMLButtonElement>('#env-tab-openai-api')!
  assert.ok(tab, '導覽多一個「OpenAI API」')
  assert.equal(tab.querySelector('.env-nav-label')?.textContent, 'OpenAI API')
  assert.ok(root.querySelector('#env-tab-claude-api'), 'Claude API 分頁照留')
  assert.equal(root.querySelector('#env-panel-claude-api [data-testid=openai-api-settings]'), null, 'Claude 分頁裡沒有 OpenAI 設定區')
  assert.ok(root.querySelector('#env-panel-openai-api [data-testid=openai-api-settings]'), 'OpenAI 設定區在自己的分頁')
  await click(tab)
  assert.equal(tab.getAttribute('aria-selected'), 'true')
  assert.equal(root.querySelector<HTMLElement>('#env-panel-openai-api')!.hidden, false)
  assert.equal(root.querySelector<HTMLElement>('#env-panel-claude-api')!.hidden, true)
})

test('OpenAI 分頁有「設定｜帳本」，帳本只抓 provider=openai 並用短名＋tooltip', async () => {
  const { OpenAiApiPanel } = await import('./OpenAiApiPanel')
  const urls: string[] = []
  const rows = [{ at: '2026-11-18T08:00:00.000Z', use: 'message_translate', model: 'gpt-6-luna', provider: 'openai', ms: 100, ok: true, error_kind: null, input_tokens: 7, output_tokens: 3, stop_reason: null }]
  globalThis.fetch = (async (input: string) => {
    urls.push(String(input))
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/openai-api/settings') return new Response(JSON.stringify(base), { status: 200 })
    if (path === '/claude-api/ledger') return new Response(JSON.stringify({ totals: { calls: 1, ok: 1, errors: 0, input_tokens: 7, output_tokens: 3 }, by_use: {}, rows }), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  const root = await mount(<OpenAiApiPanel />)
  await settle()
  const tabs = [...root.querySelectorAll<HTMLButtonElement>('button[role=tab]')]
  assert.deepEqual(tabs.map((t) => t.textContent), ['設定', '帳本'])
  await click(tabs[1]!)
  await until(() => root.querySelector('[data-testid=openai-api-totals]') !== null, '帳本載入')
  assert.ok(urls.some((u) => u.includes('/claude-api/ledger') && u.includes('provider=openai')))
  const cell = root.querySelector<HTMLElement>('#openai-book td[data-label="模型"]')!
  assert.equal(cell.textContent, 'Luna')
  assert.equal(cell.title, 'gpt-6-luna')
})

test('Claude 分頁翻譯組的供應商旁有連到 OpenAI API 分頁的連結', async () => {
  let opened = 0
  const settings = {
    enabled: true, key_present: true, key_error: null, models: ['claude-haiku-5-5'], efforts: ['low'], uses: [],
    groups: [{ id: 'translate', label: '翻譯', uses: ['message_translate'], enabled: true, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 0, auto: null, provider: 'anthropic', openai_allowed: true, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 0 }],
    openai: { available: true, unavailable_reason: null, models: ['gpt-6-luna'], efforts: ['low'] },
  }
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/claude-api/settings') return new Response(JSON.stringify(settings), { status: 200 })
    if (path === '/claude-api/ledger') return new Response(JSON.stringify({ totals: {}, by_use: {}, rows: [] }), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  const root = await mount(<ClaudeApiPanel onOpenOpenAi={() => { opened += 1 }} />)
  await settle()
  const link = root.querySelector<HTMLButtonElement>('[data-testid=claude-api-open-openai-translate]')!
  assert.ok(link, '翻譯組有連結')
  await click(link)
  assert.equal(opened, 1)
  assert.equal(root.querySelector('[data-testid=openai-api-settings]'), null)
})

test('單回合成本上限：美元輸入存成微美元；0／負數不能存（按鈕停用）；留空清除', async () => {
  const puts: unknown[] = []
  const withTurn = { ...base, key_present: true, key_error: null, api_bot: { ...base.api_bot, max_turn_cost_microusd: 2_000_000 } }
  stub(withTurn, puts, { ...withTurn, api_bot: { ...base.api_bot, max_turn_cost_microusd: 3_500_000 } })
  const root = await mount(<OpenAiApiSettings />)
  await settle()
  const turn = root.querySelector<HTMLInputElement>('[data-testid=openai-api-turn-cap]')!
  assert.equal(turn.value, '2', '已存的微美元以美元顯示')
  await typeInto(turn, '3.5')
  submit(root)
  await settle()
  assert.deepEqual(puts, [{ max_turn_cost_microusd: 3_500_000 }])

  await typeInto(root.querySelector<HTMLInputElement>('[data-testid=openai-api-turn-cap]')!, '0')
  assert.ok(root.querySelector('[data-testid=openai-api-turn-cap-invalid]'), '0 要提示')
  assert.equal(root.querySelector<HTMLButtonElement>('[data-testid=openai-api-save]')!.disabled, true)
  submit(root)
  await settle()
  assert.equal(puts.length, 1, '0 不送出')

  await typeInto(root.querySelector<HTMLInputElement>('[data-testid=openai-api-turn-cap]')!, '-1')
  assert.equal(root.querySelector<HTMLButtonElement>('[data-testid=openai-api-save]')!.disabled, true, '負數不能存')

  await typeInto(root.querySelector<HTMLInputElement>('[data-testid=openai-api-turn-cap]')!, '')
  submit(root)
  await settle()
  assert.deepEqual(puts[1], { max_turn_cost_microusd: null }, '留空＝清除')
})
