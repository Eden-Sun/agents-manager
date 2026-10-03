/**
 * 分享頁（SPEC「分享 bot」）：對話、送出後「思考中」→ 回覆、上傳附件跟著訊息送、bot 給的檔案清單、連結失效畫面；
 * 頁面上沒有任何通往主 UI 的連結。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import { httpShareClient, type ShareClient, type ShareEvents } from './shareApi'
import { mockShareClient } from './shareMock'
import type { ShareMessage, ShareStatus } from './shareModel'

const POLL_WAIT = 4200

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

test('載入對話與檔案；沒有通往主 UI 的連結', async () => {
  await mount(<ShareApp client={mockShareClient(TOKEN)} />)
  await settle(300)
  assert.equal(document.querySelector('.sh-title h1')!.textContent, 'support-bot')
  assert.equal(document.querySelectorAll('.sh-msg').length, 3)
  assert.deepEqual([...document.querySelectorAll('.sh-file-name')].map((x) => x.textContent), ['config.toml', '安裝步驟.md'])
  for (const a of document.querySelectorAll('a')) {
    const href = a.getAttribute('href') ?? ''
    assert.ok(href.startsWith('blob:') || href.startsWith('/s/') || /^https?:\/\//.test(href), `可疑連結 ${href}`)
    assert.ok(!/\/api\/(state|bots|projects)|:7788/.test(href), `不能連到主 UI：${href}`)
  }
})

test('送出：輸入框清空、出現思考中，回覆到了思考中消失', async () => {
  await mount(<ShareApp client={mockShareClient(TOKEN)} />)
  await settle(300)
  const ta = document.querySelector('textarea')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
    set.call(ta, '請問退款怎麼申請？')
    ta.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await click(document.querySelector('.sh-send')!)
  await settle(500)
  assert.equal(ta.value, '')
  assert.ok(document.querySelector('.sh-thinking'), '送出後要看得到思考中')
  assert.match([...document.querySelectorAll('.sh-msg.user')].at(-1)!.textContent!, /退款/)
  await settle(2600)
  assert.equal(document.querySelector('.sh-thinking'), null)
  assert.match([...document.querySelectorAll('.sh-msg.assistant')].at(-1)!.textContent!, /mock 的回覆/)
})

test('超過一頁時可載入較早訊息，不重複、不把人捲回最底', async () => {
  const calls: Array<string | undefined> = []
  const older = [0, 1].map((i) => ({
    id: `old-${i}`,
    role: 'user' as const,
    text: `前文 ${i}`,
    created_at: new Date(Date.UTC(2020, 0, 1, 0, i)).toISOString(),
    attachments: [] as { name: string }[],
  }))
  const recent = [0, 1].map((i) => ({
    id: `new-${i}`,
    role: 'assistant' as const,
    text: `近文 ${i}`,
    created_at: new Date(Date.UTC(2024, 0, 1, 0, i)).toISOString(),
    attachments: [] as { name: string }[],
  }))
  const client: ShareClient = {
    async messages(before) {
      calls.push(before)
      if (!before) return { bot_name: 'b', status: 'idle', messages: recent, has_more: true }
      return { bot_name: 'b', status: 'idle', messages: older, has_more: false }
    },
    async send() {},
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      return []
    },
    fileUrl: () => '/s/t/api/files/a',
    subscribe: () => () => {},
  }
  await mount(<ShareApp client={client} />)
  await settle(50)
  const btn = [...document.querySelectorAll('button')].find((b) => /較早/.test(b.textContent ?? ''))
  assert.ok(btn, 'has_more 時要能載入較早訊息')
  const list = document.querySelector('.sh-list') as HTMLElement
  Object.defineProperty(list, 'scrollHeight', { configurable: true, value: 2000 })
  Object.defineProperty(list, 'clientHeight', { configurable: true, value: 400 })
  list.scrollTop = 0
  await act(async () => {
    list.dispatchEvent(new Event('scroll', { bubbles: true }))
  })
  await click(btn!)
  await settle(50)
  assert.equal(calls[1], 'new-0', '第二頁要用目前最舊的 id 當 before')
  const texts = [...document.querySelectorAll('.sh-msg')].map((n) => n.textContent)
  assert.equal(texts.length, 4)
  assert.ok(texts.some((t) => t?.includes('前文 0')) && texts.some((t) => t?.includes('近文 1')))
  assert.equal(new Set(texts).size, texts.length)
  assert.equal(list.scrollTop, 0, '人在看舊訊息時不能被拉回最底')
  assert.equal([...document.querySelectorAll('button')].some((b) => /較早/.test(b.textContent ?? '')), false)
})

test('送出期間 SSE 先到齊，不能在 POST resolve 之後又卡回思考中', async () => {
  let status: ShareStatus = 'idle'
  const messages: ShareMessage[] = []
  let subs: ShareEvents[] = []
  const client: ShareClient = {
    async messages() {
      return { bot_name: 'b', status, messages: [...messages], has_more: false }
    },
    async send(text) {
      status = 'working'
      for (const s of subs) s.onStatus('working')
      messages.push({ id: 'u1', role: 'user', text, created_at: new Date().toISOString(), attachments: [] })
      for (const s of subs) s.onMessage(messages[0])
      messages.push({ id: 'a1', role: 'assistant', text: '已經回了', created_at: new Date().toISOString(), attachments: [] })
      for (const s of subs) s.onMessage(messages[1])
      status = 'idle'
      for (const s of subs) s.onStatus('idle')
    },
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      return []
    },
    fileUrl: () => '/s/t/api/files/a',
    subscribe(ev) {
      subs.push(ev)
      return () => {
        subs = subs.filter((s) => s !== ev)
      }
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(20)
  const ta = document.querySelector('textarea')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
    set.call(ta, '快問')
    ta.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await click(document.querySelector('.sh-send')!)
  await settle(30)
  assert.equal(document.querySelector('.sh-thinking'), null)
  assert.match(document.querySelector('.sh-state')!.textContent!, /在線/)
  assert.match(document.body.textContent!, /已經回了/)
})

test('EventSource 停在 CONNECTING 也要改輪詢；404 關流，恢復 open 就停輪詢', { timeout: 20_000 }, async () => {
  const sources: FakeSource[] = []
  class FakeSource {
    static CONNECTING = 0
    static OPEN = 1
    static CLOSED = 2
    readyState = 0
    url: string
    onerror: (() => void) | null = null
    onopen: (() => void) | null = null
    closed = false
    constructor(url: string) {
      this.url = url
      sources.push(this)
    }
    addEventListener() {}
    close() {
      this.closed = true
      this.readyState = FakeSource.CLOSED
    }
    emitOpen() {
      this.readyState = FakeSource.OPEN
      this.onopen?.()
    }
    emitError() {
      this.readyState = FakeSource.CONNECTING
      this.onerror?.()
    }
  }
  const orig = globalThis.EventSource
  globalThis.EventSource = FakeSource as unknown as typeof EventSource
  const calls: string[] = []
  let status = 200
  const origFetch = globalThis.fetch
  globalThis.fetch = (async (url: string) => {
    calls.push(String(url))
    if (status === 404) return new Response('', { status: 404 })
    return new Response(JSON.stringify({ bot_name: 'b', status: 'idle', messages: [{ id: 'm', role: 'assistant', content: 'hi', created_at: '2024-01-01T00:00:00Z' }], has_more: false }), { status: 200 })
  }) as typeof fetch
  try {
    await mount(<ShareApp client={httpShareClient('tok_0123456789abcdef')} />)
    await settle(30)
    assert.equal(sources.length, 1)
    await act(async () => {
      sources[0].emitOpen()
      sources[0].emitError()
    })
    const before = calls.length
    await settle(POLL_WAIT)
    assert.ok(calls.length > before, 'CONNECTING 的 error 也要開始輪詢')
    status = 404
    await settle(POLL_WAIT)
    assert.match(document.body.textContent!, /這個分享連結已失效/)
    assert.equal(sources[0].closed, true)
  } finally {
    globalThis.EventSource = orig
    globalThis.fetch = origFetch
  }
})

test('輪詢恢復後 EventSource 又 open，就不要一直打 messages', { timeout: 20_000 }, async () => {
  const sources: { emitOpen: () => void; emitError: () => void }[] = []
  class FakeSource {
    readyState = 0
    onerror: (() => void) | null = null
    onopen: (() => void) | null = null
    constructor(_url: string) {
      sources.push(this)
    }
    addEventListener() {}
    close() {}
    emitOpen() {
      this.readyState = 1
      this.onopen?.()
    }
    emitError() {
      this.readyState = 0
      this.onerror?.()
    }
  }
  const orig = globalThis.EventSource
  globalThis.EventSource = FakeSource as unknown as typeof EventSource
  let n = 0
  const origFetch = globalThis.fetch
  globalThis.fetch = (async () => {
    n++
    return new Response(JSON.stringify({ bot_name: 'b', status: 'idle', messages: [], has_more: false }), { status: 200 })
  }) as typeof fetch
  try {
    await mount(<ShareApp client={httpShareClient('tok_0123456789abcdef')} />)
    await settle(20)
    await act(async () => {
      sources[0].emitError()
    })
    await settle(POLL_WAIT)
    const mid = n
    assert.ok(mid > 2, 'error 之後要有輪詢')
    await act(async () => {
      sources[0].emitOpen()
    })
    await settle(POLL_WAIT)
    assert.equal(n, mid, 'SSE 恢復後停止輪詢')
  } finally {
    globalThis.EventSource = orig
    globalThis.fetch = origFetch
  }
})

test('連結失效（404）：只有失效說明，沒有輸入框', async () => {
  await mount(<ShareApp client={mockShareClient('expired_0123456789abcdef')} />)
  await settle(300)
  assert.match(document.body.textContent!, /這個分享連結已失效/)
  assert.equal(document.querySelector('textarea'), null)
})
