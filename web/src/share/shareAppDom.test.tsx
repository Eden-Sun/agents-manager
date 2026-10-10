/**
 * 分享頁（SPEC「分享 bot」）：對話、送出後「思考中」→ 回覆、上傳附件跟著訊息送、bot 給的檔案清單、連結失效畫面；
 * 頁面上沒有任何通往主 UI 的連結。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import { httpShareClient, type ShareClient, type ShareEvents } from './shareApi'
import { mockShareClient } from './shareMock'
import { ShareHttpError, type ShareMessage, type ShareStatus } from './shareModel'

const POLL_WAIT = 4200

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

test('載入對話與檔案；沒有通往主 UI 的連結', async () => {
  await mount(<ShareApp client={mockShareClient(TOKEN)} />)
  await settle(300)
  assert.equal(document.querySelector('.sh-title h1')!.textContent, 'support-bot')
  assert.equal(document.querySelectorAll('.sh-msg').length, 4)
  assert.deepEqual([...document.querySelectorAll('.sh-file-name')].map((x) => x.textContent), ['config.toml', '安裝步驟.md', '星期日早安圖卡'])
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
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: async () => new Blob([]),
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
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: async () => new Blob([]),
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
    await settle(50)
    // #1093：斷過又連上補抓一次（訊息＋檔案各一發）。
    const afterUp = n
    assert.equal(afterUp, mid + 2, 'SSE 重新連上補抓一次訊息與檔案')
    await settle(POLL_WAIT)
    assert.equal(n, afterUp, 'SSE 恢復後停止輪詢')
  } finally {
    globalThis.EventSource = orig
    globalThis.fetch = origFetch
  }
})

async function typeText(text: string) {
  const ta = document.querySelector('textarea')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
    set.call(ta, text)
    ta.dispatchEvent(new Event('input', { bubbles: true }))
  })
  return ta
}

test('bot 思考中（含 blocked）：送出鈕停用、寫「等 bot 回完再送」，回完才能送', async () => {
  let live: ShareEvents | null = null
  const base = mockShareClient(TOKEN)
  const client: ShareClient = { ...base, subscribe: (ev) => ((live = ev), () => {}) }
  await mount(<ShareApp client={client} />)
  await settle(300)
  await typeText('下一題')
  const send = document.querySelector<HTMLButtonElement>('.sh-send')!
  assert.equal(send.disabled, false)
  await act(async () => live!.onStatus('working' as ShareStatus))
  assert.equal(send.disabled, true)
  assert.match(document.querySelector('.sh-wait')!.textContent!, /等 bot 回完再送/)
  await act(async () => live!.onStatus('idle' as ShareStatus))
  assert.equal(send.disabled, false)
  assert.equal(document.querySelector('.sh-wait'), null)
})

test('還是撞到 409（上一則還在排）：字留在輸入框、提示等回完再送', async () => {
  const base = mockShareClient(TOKEN)
  let sent = 0
  const client: ShareClient = {
    ...base,
    send: async () => {
      sent++
      throw new ShareHttpError(409)
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const ta = await typeText('這段很長的問題不能被吃掉')
  await click(document.querySelector('.sh-send')!)
  await settle(300)
  assert.equal(sent, 1)
  assert.equal(ta.value, '這段很長的問題不能被吃掉')
  assert.match(document.querySelector('.sh-send-err')!.textContent!, /等 bot 回完再送；你打的字還在/)
  assert.equal(document.querySelector('.sh-thinking'), null, '沒送出去就不該卡在思考中')
})

test('連結失效（404）：只有失效說明，沒有輸入框', async () => {
  await mount(<ShareApp client={mockShareClient('expired_0123456789abcdef')} />)
  await settle(300)
  assert.match(document.body.textContent!, /這個分享連結已失效/)
  assert.equal(document.querySelector('textarea'), null)
})

test('圖片：bot 那一回合做的圖直接畫在回覆下面，按鈕是「分享／存到手機」，畫面上沒有格式名稱或技術字', async () => {
  // 轉檔停在進行中：按鈕先出現（轉好前 disabled）。happy-dom 沒有 canvas，不讓它走到失敗。
  await mount(<ShareApp client={{ ...mockShareClient(TOKEN), fileBlob: () => new Promise<Blob>(() => {}) }} />)
  await settle(300)
  const bubble = [...document.querySelectorAll('.sh-msg.assistant')].at(-1)!
  assert.ok(bubble.querySelector('.sh-imgs .sh-thumb.big img'), 'bot 這一回合做的圖要直接畫在那則回覆下面')
  assert.equal(bubble.querySelector('.sh-png-btn')?.textContent, '存到手機', '桌機（不能分享檔案）退回「存到手機」')
  assert.equal(document.querySelectorAll('.sh-msg.assistant')[0].querySelector('.sh-imgs'), null, '別回合的回覆不掛這張圖')
  const row = [...document.querySelectorAll('.sh-file-list li')].find((li) => li.textContent?.includes('星期日早安圖卡'))!
  assert.ok(row.querySelector('.sh-thumb img'), '清單上的圖要有縮圖')
  assert.equal(row.querySelector('a'), null, '圖不給原檔連結')
  assert.equal(row.querySelector('.sh-png-btn')?.textContent, '存到手機')
  assert.equal([...document.querySelectorAll('.sh-file-list li')].filter((li) => li.querySelector('.sh-thumb')).length, 1, '非圖片沒有縮圖')
  for (const img of document.querySelectorAll('img')) assert.ok(/^blob:|\/s\//.test(img.getAttribute('src') ?? ''), '圖只經 <img> 的 blob／同源網址')
  assert.equal(document.querySelector('svg text'), null, '向量圖內容不能進 DOM')

  await click(row.querySelector('.sh-thumb')!)
  const viewer = document.querySelector('.sh-viewer')!
  assert.ok(viewer, '點縮圖放大')
  assert.equal(viewer.querySelector('.sh-viewer-name')!.textContent, '星期日早安圖卡')
  assert.equal(viewer.querySelector('.sh-png-btn')!.textContent, '存到手機')
  assert.equal(viewer.querySelector('a'), null, '放大檢視也不給原檔')
  // 長輩看的畫面：任何格式名稱或技術字都不准出現（看得到的字與按鈕說明）。
  const visible = `${document.body.textContent} ${[...document.querySelectorAll('[aria-label],[title]')].map((x) => `${x.getAttribute('aria-label') ?? ''} ${x.getAttribute('title') ?? ''}`).join(' ')}`
  for (const word of ['PNG', 'SVG', 'png', 'svg', '下載', '格式', '擁有者', '管理者', '後台', '分享使用者']) {
    assert.ok(!visible.includes(word), `畫面上出現了「${word}」`)
  }
  await act(async () => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
  })
  assert.equal(document.querySelector('.sh-viewer'), null, 'Esc 關掉')
})

test('能分享檔案的手機上，按鈕叫「分享」', async () => {
  const origMatch = window.matchMedia
  const nav = navigator as Navigator & { canShare?: unknown; share?: unknown }
  const origShare = nav.share
  const origCan = nav.canShare
  window.matchMedia = ((q: string) => ({ matches: q.includes('coarse'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  Object.defineProperty(nav, 'share', { value: async () => {}, configurable: true })
  Object.defineProperty(nav, 'canShare', { value: () => true, configurable: true })
  try {
    const card = { name: '早安.png', size: 3, modified_at: null }
    await mount(<ShareApp client={{ ...mockShareClient(TOKEN), files: async () => [card], fileBlob: async () => new Blob(['png'], { type: 'image/png' }) }} />)
    await settle(300)
    assert.equal(document.querySelector('.sh-file-list .sh-png-btn')!.textContent, '分享')
  } finally {
    window.matchMedia = origMatch
    Object.defineProperty(nav, 'share', { value: origShare, configurable: true })
    Object.defineProperty(nav, 'canShare', { value: origCan, configurable: true })
  }
})

test('引用外部資源的向量圖：不給按鈕，只說沒辦法分享', async () => {
  const base = mockShareClient(TOKEN)
  const evil = '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image href="https://evil.example/x.png"/></svg>'
  const client: ShareClient = {
    ...base,
    files: async () => [{ name: 'ext.svg', size: evil.length, modified_at: null }],
    fileBlob: async () => new Blob([evil]),
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const row = document.querySelector('.sh-file-list li')!
  assert.equal(row.querySelector('.sh-png-btn'), null)
  assert.equal(row.querySelector('.sh-png-note')!.textContent, '這張圖沒辦法分享')
})

test('載不出來的圖（bot 寫壞、網路斷）：講「還在修」不嚇人、不給按鈕；跟引用外部資源的講法不同', async () => {
  const base = mockShareClient(TOKEN)
  // 標籤沒配好（測試環境的 DOMParser 認得的那種壞法；真瀏覽器連屬性之間少空格也認得）。
  const broken = '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><g><text x="5" y="5">嗨</text></svg>'
  const evil = '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image href="https://evil.example/x.png"/></svg>'
  const client: ShareClient = {
    ...base,
    files: async () => [
      { name: 'broken.svg', size: broken.length, modified_at: '2026-10-04T00:00:00Z' },
      { name: 'offline.png', size: 3, modified_at: '2026-10-04T00:00:00Z' },
      { name: 'ext.svg', size: evil.length, modified_at: '2026-10-04T00:00:00Z' },
    ],
    fileBlob: async (name) => {
      if (name === 'offline.png') throw new Error('network')
      return new Blob([name === 'ext.svg' ? evil : broken])
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const note = (name: string) => [...document.querySelectorAll('.sh-file-list li')].find((li) => li.textContent?.includes(name.replace(/\.\w+$/, '')))!.querySelector('.sh-png-note')?.textContent
  assert.equal(note('broken.svg'), '這張圖還在修，請稍等')
  assert.equal(note('offline.png'), '這張圖還在修，請稍等')
  assert.equal(note('ext.svg'), '這張圖沒辦法分享', '外部資源照舊')
  assert.equal(document.querySelectorAll('.sh-file-list .sh-png-btn').length, 0, '都不給按鈕')
})

test('檔案換版（bot 修好、SSE resync 重抓清單）就重試：「還在修」變回按鈕', async () => {
  let ev: ShareEvents | null = null
  let fixed = false
  const client: ShareClient = {
    ...mockShareClient(TOKEN),
    files: async () => [{ name: '早安.png', size: fixed ? 4 : 3, modified_at: fixed ? '2026-10-04T00:01:00Z' : '2026-10-04T00:00:00Z' }],
    fileBlob: async () => {
      if (!fixed) throw new Error('decode')
      return new Blob(['png!'], { type: 'image/png' })
    },
    subscribe(e) {
      ev = e
      return () => {}
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  assert.equal(document.querySelector('.sh-file-list .sh-png-note')!.textContent, '這張圖還在修，請稍等')
  fixed = true
  await act(async () => {
    ev!.onResync?.()
  })
  await settle(300)
  assert.ok(document.querySelector('.sh-file-list .sh-png-btn'), '修好的那一版有按鈕')
  assert.equal(document.querySelector('.sh-file-list .sh-png-note'), null)
})

test('同秒同大小重寫（時間、大小都沒變，只有 version 變）也算換版：重抓檔案、「還在修」變回按鈕 (issue #843)', async () => {
  let ev: ShareEvents | null = null
  let fixed = false
  let fetches = 0
  const client: ShareClient = {
    ...mockShareClient(TOKEN),
    files: async () => [{ name: '早安.png', size: 3, modified_at: '2026-10-04T00:00:00Z', version: fixed ? 'v2' : 'v1' }],
    fileBlob: async () => {
      fetches++
      if (!fixed) throw new Error('decode')
      return new Blob(['png'], { type: 'image/png' })
    },
    subscribe(e) {
      ev = e
      return () => {}
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  assert.equal(document.querySelector('.sh-file-list .sh-png-note')!.textContent, '這張圖還在修，請稍等')
  const before = fetches
  fixed = true
  await act(async () => {
    ev!.onResync?.()
  })
  await settle(300)
  assert.ok(fetches > before, 'version 變了就重抓')
  assert.ok(document.querySelector('.sh-file-list .sh-png-btn'), '修好的那一版有按鈕')
  assert.equal(document.querySelector('.sh-file-list .sh-png-note'), null)
})

test('resync（對話被倒回）：整頁重抓並取代手上的清單；前綴不顯示', async () => {
  let ev: ShareEvents | null = null
  let page: ShareMessage[] = [
    { id: 'a', role: 'user', text: '〔分享使用者〕 早安', created_at: '2026-10-04T00:00:00Z', attachments: [] },
    { id: 'b', role: 'assistant', text: '早安！', created_at: '2026-10-04T00:00:01Z', attachments: [] },
  ]
  const client: ShareClient = {
    ...mockShareClient(TOKEN),
    messages: async () => ({ bot_name: 'b', status: 'idle', messages: page, has_more: false }),
    files: async () => [],
    subscribe(e) {
      ev = e
      return () => {}
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(100)
  assert.equal(document.querySelectorAll('.sh-msg').length, 2)
  assert.equal(document.querySelector('.sh-msg.user')!.textContent!.includes('分享使用者'), false, '她看到的是自己打的原文')
  page = [page[0]]
  await act(async () => {
    ev!.onResync?.()
  })
  await settle(50)
  assert.equal(document.querySelectorAll('.sh-msg').length, 1, '倒回掉的那則要消失')
})

/** 客訴 2026-10-04：一次選 5 個檔，原本同時送出、第三張起「傳得太快了」。現在一張接一張、看得到第幾張／共幾張，429 自己重試。 */
test('一次選好幾個檔：依序上傳、顯示第幾張，429 自動重試、不顯示「太快」', async () => {
  let inFlight = 0
  let maxInFlight = 0
  let calls = 0
  const order: string[] = []
  const client = mockShareClient(TOKEN)
  client.upload = async (f: File) => {
    calls++
    inFlight++
    maxInFlight = Math.max(maxInFlight, inFlight)
    await new Promise((r) => setTimeout(r, 150))
    inFlight--
    if (calls === 2) throw new ShareHttpError(429, 1)
    order.push(f.name)
    return { id: `att-${f.name}`, name: f.name }
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  const picked = [1, 2, 3, 4, 5].map((i) => new File([`photo ${i}`], `p${i}.txt`, { type: 'text/plain' }))
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: picked })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await settle(20)
  assert.match(document.querySelector('.sh-pending')!.textContent!, /第 1 張／共 5 張/)
  assert.match(document.querySelector('.sh-pending')!.textContent!, /等待中/)
  // act 外完成的上傳要到下一個 act 才 render：分段等，每段都讓佇列往前走。
  for (let i = 0; i < 40 && document.querySelectorAll('.sh-pending li.ok').length < 5; i++) await settle(100)
  assert.equal(maxInFlight, 1, '一次只傳一張')
  assert.deepEqual(order, ['p1.txt', 'p2.txt', 'p3.txt', 'p4.txt', 'p5.txt'])
  assert.equal(calls, 6, '第 2 張 429 之後自己再傳一次')
  assert.doesNotMatch(document.body.textContent!, /太快/)
  assert.equal(document.querySelectorAll('.sh-pending li.ok').length, 5)
})

test('真的沒傳上去：寫「這張沒傳上去」，按「再試一次」重傳那一張', async () => {
  let fail = true
  const client = mockShareClient(TOKEN)
  client.upload = async (f: File) => {
    if (fail) throw new Error('network down')
    return { id: 'att-x', name: f.name }
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: [new File(['x'], 'x.txt', { type: 'text/plain' })] })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await settle(50)
  await settle(50)
  assert.match(document.querySelector('.sh-pending')!.textContent!, /這張沒傳上去/)
  fail = false
  await click(document.querySelector('.sh-retry')!)
  await settle(50)
  await settle(50)
  assert.equal(document.querySelectorAll('.sh-pending li.ok').length, 1)
  assert.equal(document.querySelector('.sh-retry'), null)
})

/** 送出的 POST 先等 gate 才交給 mock：送出期間輸入框與📎都沒有停用，可以接著打字、選檔。 */
function gatedSendClient(): { client: ShareClient; release: () => void; sent: Array<{ text: string; attachments: string[] }>; uploaded: string[] } {
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  const sent: Array<{ text: string; attachments: string[] }> = []
  const uploaded: string[] = []
  const base = mockShareClient(TOKEN)
  const client: ShareClient = {
    ...base,
    async send(text, clientRequestId, attachments) {
      sent.push({ text, attachments })
      await gate
      return base.send(text, clientRequestId, attachments)
    },
    async upload(file) {
      const r = await base.upload(file)
      uploaded.push(r.id)
      return r
    },
  }
  return { client, release, sent, uploaded }
}

test('送出中打的下一則不會被清掉', async () => {
  const { client, release, sent } = gatedSendClient()
  try {
    await mount(<ShareApp client={client} />)
    await settle(300)
    const ta = document.querySelector('textarea')!
    const setTa = (v: string) =>
      act(async () => {
        const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
        set.call(ta, v)
        ta.dispatchEvent(new Event('input', { bubbles: true }))
      })
    await setTa('第一則')
    await click(document.querySelector('.sh-send')!)
    await settle(50)
    await setTa('第二則')
    release()
    await settle(300)
    assert.equal(ta.value, '第二則')
    assert.equal(sent[0].text, '第一則')
  } finally {
    release()
  }
})

test('送出中才傳好的附件留著', async () => {
  const { client, release, sent, uploaded } = gatedSendClient()
  try {
    await mount(<ShareApp client={client} />)
    await settle(300)
    const ta = document.querySelector('textarea')!
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
      set.call(ta, '第一則')
      ta.dispatchEvent(new Event('input', { bubbles: true }))
    })
    await click(document.querySelector('.sh-send')!)
    await settle(50)
    // 送出期間才選的檔：傳好（拿到 id）但沒有跟這一則送出。
    const fi = document.querySelector<HTMLInputElement>('input[type="file"]')!
    Object.defineProperty(fi, 'files', { configurable: true, value: [new File(['hi'], 'late.png', { type: 'image/png' })] })
    await act(async () => fi.dispatchEvent(new Event('change', { bubbles: true })))
    await until(() => document.querySelector('.sh-pending li.ok') !== null, '附件傳好')
    release()
    await settle(300)
    assert.equal(sent[0].attachments.length, 0, '第一則送出時沒帶附件')
    assert.equal(document.querySelectorAll('.sh-pending li').length, 1, '送出期間才傳好的附件還在清單上')
    assert.equal(document.querySelector('.sh-pending li')!.className, 'ok')
    // 等 bot 回完、送出鈕恢復，再按送出：這次附件要跟上。
    await until(() => !document.querySelector<HTMLButtonElement>('.sh-send')!.disabled, '可以再送')
    await click(document.querySelector('.sh-send')!)
    await settle(300)
    assert.deepEqual(sent[1].attachments, uploaded)
  } finally {
    release()
  }
})

/** #1092 的測試共用：一頁訊息，與只有 messages 會變的自製 client。 */
const page = (ms: ShareMessage[]) => ({ bot_name: 'b', status: 'idle' as const, has_more: false, messages: ms })
const msgAt = (id: string, role: 'user' | 'assistant', n: number): ShareMessage => ({
  id,
  role,
  text: id,
  created_at: new Date(Date.UTC(2026, 9, 4, 0, n)).toISOString(),
  attachments: [],
})
function clientWith(messages: () => Promise<ReturnType<typeof page>>, subscribe: (e: ShareEvents) => () => void): ShareClient {
  return {
    messages,
    async send() {},
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      return []
    },
    fileUrl: () => '/s/t/api/files/a',
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: async () => new Blob([]),
    subscribe,
  }
}

test('第一次載入失敗而 SSE 連著：自己重試，不會停在「載入中…」', { timeout: 20_000 }, async () => {
  let calls = 0
  const client = clientWith(
    async () => {
      calls++
      if (calls === 1) throw new ShareHttpError(503, 5)
      return page([msgAt('a1', 'assistant', 2)])
    },
    (e) => {
      e.onUp?.()
      return () => {}
    },
  )
  await mount(<ShareApp client={client} />)
  await settle(50)
  assert.match(document.querySelector('.sh-net')!.textContent!, /連不上/)
  assert.match(document.querySelector('.sh-list')!.textContent!, /載入中…/)
  await settle(POLL_WAIT)
  assert.equal(document.querySelectorAll('.sh-msg').length, 1)
  assert.equal(document.querySelector('.sh-net'), null)
  assert.doesNotMatch(document.querySelector('.sh-list')!.textContent!, /載入中…/)
  assert.equal(calls, 2)
  await settle(POLL_WAIT)
  assert.equal(calls, 2, '成功後不再打')
})

test('resync 的重抓失敗：重試成功後仍以重抓結果取代清單', { timeout: 20_000 }, async () => {
  let calls = 0
  let ev!: ShareEvents
  const client = clientWith(
    async () => {
      calls++
      if (calls === 1) return page([msgAt('a', 'user', 1), msgAt('b', 'assistant', 2)])
      if (calls === 2) throw new ShareHttpError(503, 5)
      return page([msgAt('a', 'user', 1)])
    },
    (e) => {
      ev = e
      return () => {}
    },
  )
  await mount(<ShareApp client={client} />)
  await settle(100)
  assert.equal(document.querySelectorAll('.sh-msg').length, 2)
  await act(async () => {
    ev.onResync?.()
  })
  await settle(50)
  assert.equal(document.querySelectorAll('.sh-msg').length, 2, '重抓失敗前清單不能被丟掉')
  assert.ok(document.querySelector('.sh-net'))
  await settle(POLL_WAIT)
  assert.equal(document.querySelectorAll('.sh-msg').length, 1, '重試成功後以重抓結果取代')
  assert.equal(document.querySelector('.sh-net'), null)
})

test('輪詢開著時失敗不另外多排一條重試', { timeout: 20_000 }, async () => {
  let calls = 0
  const client = clientWith(
    async () => {
      calls++
      throw new ShareHttpError(503, 5)
    },
    (e) => {
      e.onDown()
      return () => {}
    },
  )
  await mount(<ShareApp client={client} />)
  await settle(POLL_WAIT * 2 + 300)
  assert.ok(calls <= 3, `開頁 1 次＋兩個 interval tick，實際 ${calls}`)
})

/** #1093：SSE 在背景斷線又連上、回到前景：訊息與檔案各補抓一次（斷線期間的事件不會重播）。 */
function reconnectHarness() {
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
  const counts = { messages: 0, files: 0 }
  const origFetch = globalThis.fetch
  const origSource = globalThis.EventSource
  globalThis.EventSource = FakeSource as unknown as typeof EventSource
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    if (String(input).includes('/files')) {
      counts.files++
      return new Response('[]', { status: 200 })
    }
    counts.messages++
    return new Response(JSON.stringify({ bot_name: 'b', status: 'idle', messages: [], has_more: false }), { status: 200 })
  }) as typeof fetch
  let vis: DocumentVisibilityState = 'visible'
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => vis })
  const restore = () => {
    globalThis.EventSource = origSource
    globalThis.fetch = origFetch
    delete (document as { visibilityState?: string }).visibilityState
  }
  return { sources, counts, setVis: (v: DocumentVisibilityState) => (vis = v), restore }
}

test('SSE 在背景斷線又連上：連上時補抓一次訊息與檔案', { timeout: 20_000 }, async () => {
  const h = reconnectHarness()
  try {
    await mount(<ShareApp client={httpShareClient('tok_0123456789abcdef')} />)
    await settle(20)
    await act(async () => {
      h.sources[0].emitOpen()
    })
    await settle(20)
    h.setVis('hidden')
    await act(async () => {
      h.sources[0].emitError()
    })
    const m0 = h.counts.messages
    const f0 = h.counts.files
    await settle(POLL_WAIT)
    assert.equal(h.counts.messages, m0, '背景不輪詢')
    await act(async () => {
      h.sources.at(-1)!.emitOpen()
    })
    await settle(50)
    assert.equal(h.counts.messages, m0 + 1)
    assert.equal(h.counts.files, f0 + 1)
  } finally {
    h.restore()
  }
})

test('第一次連上不多抓', { timeout: 20_000 }, async () => {
  const h = reconnectHarness()
  try {
    await mount(<ShareApp client={httpShareClient('tok_0123456789abcdef')} />)
    await settle(20)
    const m0 = h.counts.messages
    const f0 = h.counts.files
    await act(async () => {
      h.sources[0].emitOpen()
    })
    await settle(50)
    assert.equal(h.counts.messages, m0)
    assert.equal(h.counts.files, f0)
  } finally {
    h.restore()
  }
})

test('回到前景補抓一次；背景時不補', { timeout: 20_000 }, async () => {
  const h = reconnectHarness()
  try {
    await mount(<ShareApp client={httpShareClient('tok_0123456789abcdef')} />)
    await settle(20)
    await act(async () => {
      h.sources[0].emitOpen()
    })
    await settle(20)
    const m0 = h.counts.messages
    h.setVis('hidden')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await settle(50)
    assert.equal(h.counts.messages, m0, '背景時回呼不抓')
    h.setVis('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await settle(50)
    assert.equal(h.counts.messages, m0 + 1)
  } finally {
    h.restore()
  }
})

/** #1094：bot 做的圖要畫在那則回覆下面；輪詢模式沒有 SSE 的 message 事件，新回覆到了也要重抓檔案清單。 */
test('輪詢模式：新回覆到了也重抓檔案清單，圖出現在那則回覆下面', { timeout: 20_000 }, async () => {
  const T = Date.UTC(2026, 9, 4)
  const iso = (ms: number) => new Date(T + ms).toISOString()
  let replied = false
  let filesCalls = 0
  const client: ShareClient = {
    async messages() {
      return {
        bot_name: 'b',
        status: 'idle',
        has_more: false,
        messages: [
          { id: 'u1', role: 'user', text: '做一張', created_at: iso(0), attachments: [] },
          ...(replied ? [{ id: 'a1', role: 'assistant' as const, text: '好了', created_at: iso(2000), attachments: [] }] : []),
        ],
      }
    },
    async send() {},
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      filesCalls++
      return replied ? [{ name: '卡片.png', size: 3, modified_at: iso(1000), version: 'v1' }] : []
    },
    fileUrl: () => '/s/t/api/files/a',
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: () => new Promise<Blob>(() => {}),
    subscribe: (e) => {
      e.onDown()
      return () => {}
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(100)
  assert.equal(document.querySelector('.sh-imgs'), null)
  assert.equal(filesCalls, 1)
  replied = true
  await settle(POLL_WAIT)
  assert.ok(document.querySelector('.sh-msg.assistant .sh-imgs .sh-thumb'), '圖要在那則回覆下面')
  assert.match(document.querySelector('.sh-files-btn')!.textContent!, /（1）/)
  const afterReply = filesCalls
  await settle(POLL_WAIT)
  assert.equal(filesCalls, afterReply, '同一則回覆不重複抓')
})

test('SSE 模式不重複抓：收到回覆事件已經抓過，載入頁面不再多抓', { timeout: 20_000 }, async () => {
  const T = Date.UTC(2026, 9, 4)
  const iso = (ms: number) => new Date(T + ms).toISOString()
  let replied = false
  let filesCalls = 0
  let ev!: ShareEvents
  const client: ShareClient = {
    async messages() {
      return {
        bot_name: 'b',
        status: 'idle',
        has_more: false,
        messages: [
          { id: 'u1', role: 'user', text: '做一張', created_at: iso(0), attachments: [] },
          ...(replied ? [{ id: 'a1', role: 'assistant' as const, text: '好了', created_at: iso(2000), attachments: [] }] : []),
        ],
      }
    },
    async send() {},
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      filesCalls++
      return replied ? [{ name: '卡片.png', size: 3, modified_at: iso(1000), version: 'v1' }] : []
    },
    fileUrl: () => '/s/t/api/files/a',
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: () => new Promise<Blob>(() => {}),
    subscribe: (e) => {
      ev = e
      return () => {}
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(100)
  replied = true
  await act(async () => {
    ev.onMessage({ id: 'a1', role: 'assistant', text: '好了', created_at: iso(2000), attachments: [] })
  })
  await settle(100)
  assert.equal(filesCalls, 2, '開頁一次＋這一則回覆一次，不是三次')
})

/** #1095：超過 20 個附件在送出前就擋（daemon 會回 400，畫面原本只寫「送出失敗」）；移到 20 個就能送。 */
test('超過 20 個附件：送出鈕停用並說明要移除幾個；移到 20 個就能送', { timeout: 20_000 }, async () => {
  const client = mockShareClient(TOKEN)
  client.upload = async (f) => ({ id: `att-${f.name}`, name: f.name })
  const sent: string[][] = []
  client.send = async (_t, _c, ids) => {
    sent.push(ids)
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  const files = Array.from({ length: 21 }, (_, i) => new File(['x'], `p${i}.txt`, { type: 'text/plain' }))
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: files })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  for (let i = 0; i < 60 && document.querySelectorAll('.sh-pending li.ok').length < 21; i++) await settle(50)
  assert.equal(document.querySelectorAll('.sh-pending li.ok').length, 21)
  assert.equal((document.querySelector('.sh-send') as HTMLButtonElement).disabled, true)
  const err = document.querySelector('.sh-send-err')!.textContent!
  assert.match(err, /最多送 20 個/)
  assert.match(err, /移除 1 個/)
  assert.equal(sent.length, 0)

  await click(document.querySelector('.sh-pending li button[aria-label^="移除"]')!)
  await settle(50)
  assert.equal(document.querySelectorAll('.sh-pending li').length, 20)
  assert.equal((document.querySelector('.sh-send') as HTMLButtonElement).disabled, false)
  assert.equal(document.querySelector('.sh-send-err'), null)
  await click(document.querySelector('.sh-send')!)
  await settle(100)
  assert.equal(sent.length, 1)
  assert.equal(sent[0].length, 20)
})

/** #1096：載入較早訊息後留在原來那一則，不跳到最舊（scrollHeight 變大、scrollTop 要跟著補）。 */
test('載入較早訊息後留在原來那一則，不跳到最舊', { timeout: 20_000 }, async () => {
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
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: async () => new Blob([]),
    subscribe: () => () => {},
  }
  await mount(<ShareApp client={client} />)
  await settle(50)
  const list = document.querySelector('.sh-list') as HTMLElement
  Object.defineProperty(list, 'scrollHeight', { configurable: true, get: () => document.querySelectorAll('.sh-msg').length * 100 })
  Object.defineProperty(list, 'clientHeight', { configurable: true, value: 100 })
  list.scrollTop = 0
  await act(async () => {
    list.dispatchEvent(new Event('scroll', { bubbles: true }))
  })
  const btn = [...document.querySelectorAll('button')].find((b) => /較早/.test(b.textContent ?? ''))!
  await click(btn)
  await settle(50)
  assert.equal(document.querySelectorAll('.sh-msg').length, 4)
  assert.equal(list.scrollTop, 200, '多出來的兩則（200px）補回 scrollTop，人留在原來那一則')
})
