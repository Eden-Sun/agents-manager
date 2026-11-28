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

type Handler = (url: string, init: RequestInit | undefined) => Response | null
const json = (b: unknown, status = 200) => new Response(JSON.stringify(b), { status, headers: { 'Content-Type': 'application/json' } })

function stubApi(handler: Handler) {
  const originalFetch = globalThis.fetch
  const seen: { url: string; token: string | undefined }[] = []
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url === '/api/session') return json({ token: 'tok-ok' })
    const token = (init?.headers as Record<string, string> | undefined)?.['X-AM-Token']
    seen.push({ url, token })
    if (url.startsWith('/api/codex/') && token !== 'tok-ok') return json({ error: 'missing or bad X-AM-Token' }, 401)
    return handler(url, init) ?? originalFetch(input, init)
  }
  return { seen, restore: () => (globalThis.fetch = originalFetch) }
}

test('CodexStopHookSettings：顯示狀態，按「信任此 hook」要先確認才送 POST', async () => {
  let status = 'untrusted'
  const posts: unknown[] = []
  const api = stubApi((url, init) => {
    if (url === '/api/codex/stop-hook' && (init?.method ?? 'GET') === 'GET') {
      return json({ enabled: true, entries: [{ host: 'local', identity: null, codex_home: '/h/.codex', status, kill_guard: status }] })
    }
    if (url === '/api/codex/stop-hook/trust' && init?.method === 'POST') {
      posts.push(JSON.parse(init.body as string))
      status = 'trusted'
      return json({ host: 'local', identity: null, codex_home: '/h/.codex', status, kill_guard: status })
    }
    return null
  })
  try {
    await mount(<CodexStopHookSettings />)
    await settle()
    assert.match(text(), /Codex Stop hook 信任/)
    assert.match(text(), /local \/ 預設帳號/)
    assert.match(text(), /未信任/)
    assert.match(text(), /kill 守門：未信任/)
    assert.match(text(), /codex kill 守門未生效/)

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
    assert.doesNotMatch(text(), /codex kill 守門未生效/)
    const authed = api.seen.filter((c) => c.token === 'tok-ok').map((c) => c.url)
    assert.ok(authed.includes('/api/codex/stop-hook'), '讀取帶 UI token')
    assert.ok(authed.includes('/api/codex/stop-hook/trust'), '信任帶 UI token')
  } finally {
    api.restore()
  }
})

test('CodexStopHookSettings：Stop 已信任但 kill 守門沒有時，仍標「未生效」並給信任按鈕', async () => {
  const api = stubApi((url) =>
    url === '/api/codex/stop-hook'
      ? json({ enabled: true, entries: [{ host: 'm4p', identity: 'co1', codex_home: '/u/.codex', status: 'trusted', kill_guard: 'not_installed' }] })
      : null,
  )
  try {
    await mount(<CodexStopHookSettings />)
    await settle()
    assert.match(text(), /Stop：已信任/)
    assert.match(text(), /kill 守門：尚未安裝/)
    assert.match(text(), /codex kill 守門未生效/)
    assert.ok([...document.querySelectorAll('button')].some((b) => b.textContent === '信任此 hook'))
  } finally {
    api.restore()
  }
})

test('CodexStopHookSettings：讀取失敗時區塊仍在，顯示錯誤並可重試', async () => {
  let fail = true
  const api = stubApi((url) => {
    if (url !== '/api/codex/stop-hook') return null
    return fail
      ? json({ error: 'upstream' }, 502)
      : json({ enabled: true, entries: [{ host: 'local', identity: null, codex_home: '/h/.codex', status: 'trusted', kill_guard: 'trusted' }] })
  })
  try {
    await mount(<CodexStopHookSettings />)
    await settle()
    assert.match(text(), /Codex Stop hook 信任/)
    assert.match(text(), /讀取 Stop hook 狀態失敗/)
    const retry = [...document.querySelectorAll('button')].find((b) => b.textContent === '重試')
    assert.ok(retry)
    fail = false
    await click(retry)
    await settle()
    assert.doesNotMatch(text(), /讀取 Stop hook 狀態失敗/)
    assert.match(text(), /local \/ 預設帳號/)
    assert.match(text(), /Stop：已信任/)
  } finally {
    api.restore()
  }
})
