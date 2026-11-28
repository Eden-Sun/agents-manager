import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { AutoUpdateSettings } from './AutoUpdateSettings'

afterEach(unmountAll)
before(async () => {
  await setupDom()
})
after(async () => {
  await teardownDom()
})

const text = () => document.body.textContent ?? ''

test('AutoUpdateSettings：載入四個開關並顯示說明文字，點擊開關送出 PUT', async () => {
  let settings = { claude: false, codex: false, agy: false, grok: false }
  const calls: { method: string; url: string; body?: unknown; token?: string }[] = []

  const originalFetch = globalThis.fetch
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    const method = init?.method ?? 'GET'
    if (url === '/api/session') return new Response(JSON.stringify({ token: 'tok-ok' }), { status: 200, headers: { 'Content-Type': 'application/json' } })
    if (url === '/api/settings/auto-update') {
      const token = (init?.headers as Record<string, string> | undefined)?.['X-AM-Token']
      if (token !== 'tok-ok') return new Response(JSON.stringify({ error: 'missing or bad X-AM-Token' }), { status: 401 })
      calls.push({ method, url, token, body: init?.body ? JSON.parse(init.body as string) : undefined })
      if (method === 'GET') {
        return new Response(JSON.stringify(settings), { status: 200, headers: { 'Content-Type': 'application/json' } })
      }
      if (method === 'PUT') {
        const partial = JSON.parse(init?.body as string)
        settings = { ...settings, ...partial }
        return new Response(JSON.stringify(settings), { status: 200, headers: { 'Content-Type': 'application/json' } })
      }
    }
    return originalFetch(input, init)
  }

  try {
    await mount(<AutoUpdateSettings />)
    await settle()

    assert.match(text(), /自動更新（不通知）/)
    assert.match(text(), /有新版自動安裝，不跳通知；跑著的 bot 重啟後才換版（codex 會重啟閒置的 codex bot）。失敗時仍會通知。/)

    const checkboxes = [...document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')]
    assert.equal(checkboxes.length, 4, '要有四個開關：claude, codex, agy, grok')
    assert.ok(checkboxes.every((cb) => !cb.checked), '初始預設全關')

    const codexBox = checkboxes[1]
    assert.ok(codexBox)
    await click(codexBox)
    await settle()

    assert.ok(calls.some((c) => c.method === 'PUT' && JSON.stringify(c.body) === JSON.stringify({ codex: true })))
    assert.equal(codexBox.checked, true)
    assert.ok(calls.some((c) => c.method === 'GET') && calls.every((c) => c.token === 'tok-ok'), 'GET／PUT 都帶 UI token')
  } finally {
    globalThis.fetch = originalFetch
  }
})

test('AutoUpdateSettings：讀取失敗顯示錯誤、開關停用，重試成功後恢復', async () => {
  let fail = true
  const originalFetch = globalThis.fetch
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url === '/api/session') return new Response(JSON.stringify({ token: 'tok-ok' }), { status: 200, headers: { 'Content-Type': 'application/json' } })
    if (url === '/api/settings/auto-update') {
      return fail
        ? new Response(JSON.stringify({ error: 'boom' }), { status: 500, headers: { 'Content-Type': 'application/json' } })
        : new Response(JSON.stringify({ claude: true, codex: false, agy: false, grok: false }), { status: 200, headers: { 'Content-Type': 'application/json' } })
    }
    return originalFetch(input, init)
  }
  try {
    await mount(<AutoUpdateSettings />)
    await settle()
    assert.match(text(), /讀取自動更新設定失敗/)
    const boxes = () => [...document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')]
    assert.ok(boxes().every((cb) => cb.disabled), '讀不到設定時不讓人亂切')
    const retry = [...document.querySelectorAll('button')].find((b) => b.textContent === '重試')
    assert.ok(retry)
    fail = false
    await click(retry)
    await settle()
    assert.doesNotMatch(text(), /讀取自動更新設定失敗/)
    assert.equal(boxes()[0].checked, true)
    assert.ok(boxes().every((cb) => !cb.disabled))
  } finally {
    globalThis.fetch = originalFetch
  }
})
