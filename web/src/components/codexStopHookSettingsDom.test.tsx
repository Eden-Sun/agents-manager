import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { CodexStopHookSettings } from './CodexStopHookSettings'

afterEach(unmountAll)
before(async () => {
  await setupDom()
})
after(async () => {
  await teardownDom()
})

const text = () => document.body.textContent ?? ''

test('CodexStopHookSettings：顯示狀態，按「信任此 hook」要先確認才送 POST', async () => {
  let status = 'untrusted'
  const posts: unknown[] = []
  const originalFetch = globalThis.fetch
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    const json = (b: unknown) => new Response(JSON.stringify(b), { status: 200, headers: { 'Content-Type': 'application/json' } })
    if (url === '/api/codex/stop-hook' && (init?.method ?? 'GET') === 'GET') {
      return json({ enabled: true, entries: [{ host: 'local', identity: null, codex_home: '/h/.codex', status }] })
    }
    if (url === '/api/codex/stop-hook/trust' && init?.method === 'POST') {
      posts.push(JSON.parse(init.body as string))
      status = 'trusted'
      return json({ host: 'local', identity: null, codex_home: '/h/.codex', status })
    }
    return originalFetch(input, init)
  }
  try {
    await mount(<CodexStopHookSettings />)
    await settle()
    assert.match(text(), /Codex Stop hook 信任/)
    assert.match(text(), /local \/ 預設帳號/)
    assert.match(text(), /未信任/)

    const trustBtn = [...document.querySelectorAll('button')].find((b) => b.textContent === '信任此 hook')
    assert.ok(trustBtn)
    await click(trustBtn)
    await settle()
    assert.equal(posts.length, 0, '按下去只開確認對話框，不直接寫入')
    assert.match(text(), /信任 AG Man 的 Codex Stop hook？/)
    assert.match(text(), /trusted_hash/)

    const confirmBtn = [...document.querySelectorAll('button')].filter((b) => b.textContent === '信任此 hook').pop()
    assert.ok(confirmBtn)
    await click(confirmBtn)
    await settle()
    assert.deepEqual(posts, [{ host: 'local', identity: null }])
    assert.match(text(), /已信任/)
  } finally {
    globalThis.fetch = originalFetch
  }
})
