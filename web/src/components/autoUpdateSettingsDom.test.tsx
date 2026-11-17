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
  const calls: { method: string; url: string; body?: unknown }[] = []

  const originalFetch = globalThis.fetch
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    const method = init?.method ?? 'GET'
    if (url === '/api/settings/auto-update') {
      calls.push({ method, url, body: init?.body ? JSON.parse(init.body as string) : undefined })
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

    // 點擊 codex 開關
    const codexBox = checkboxes[1]
    assert.ok(codexBox)
    await click(codexBox)
    await settle()

    assert.ok(calls.some((c) => c.method === 'PUT' && JSON.stringify(c.body) === JSON.stringify({ codex: true })))
    assert.equal(codexBox.checked, true)
  } finally {
    globalThis.fetch = originalFetch
  }
})
