/**
 * outbox 的 .html 預覽（使用者 2026-11-18：不沙箱化）：.html／.htm 與 .txt 有 ↗，.svg 與未列副檔名沒有；
 * 點 html 的 ↗ ＝ 帶 token 取檔 → 重包成 type 為 text/html 的 blob → 新分頁導到那個 blob URL（完整頁面，不包 iframe），
 * 分頁關閉後 revoke；其他副檔名（.txt）行為不變（走 inline 的 blob，不重包）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { OutboxFiles } from './OutboxFiles'

const realFetch = globalThis.fetch
const realCreate = URL.createObjectURL
const realRevoke = URL.revokeObjectURL
let realOpen: typeof window.open
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })
const HTML = '<h1>hi</h1><script src="https://cdn.jsdelivr.net/npm/chart.js"></script>'
const fetched: string[] = []
const revoked: string[] = []
const created: Blob[] = []
const realSetInterval = globalThis.setInterval
const realClearInterval = globalThis.clearInterval
/** 預覽用的「分頁關了沒」輪詢（30 秒）：攔下來由測試手動觸發，免得真的計時、也驗得到 revoke。 */
const polls: Array<() => void> = []
let opened: { opener: unknown; href: string; closed: boolean } | null = null

before(async () => {
  await setupDom()
  realOpen = window.open
  window.open = (() => {
    const state = { opener: window as unknown, href: '', closed: false }
    opened = state
    return {
      get closed() { return state.closed },
      close() { state.closed = true },
      get opener() { return state.opener },
      set opener(v: unknown) { state.opener = v },
      location: { get href() { return state.href }, set href(v: string) { state.href = v } },
    } as unknown as Window
  }) as typeof window.open
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    fetched.push(url)
    if (url.startsWith('blob:')) return Promise.resolve(new Response(HTML, { status: 200, headers: { 'content-type': 'application/octet-stream' } }))
    if (url.includes('/outbox/file')) return Promise.resolve(new Response(HTML, { status: 200, headers: { 'content-type': 'application/octet-stream' } }))
    return Promise.resolve(json({
      dir: '/x', reason: null, ttl_secs: 3600, kept: false,
      files: ['chart.html', 'old.htm', 'note.txt', 'logo.svg', 'data.xyz'].map((name) => ({ name, size: 3, modified: 1, remaining_secs: 1800 })),
    }))
  }) as typeof fetch
  globalThis.setInterval = (((h: () => void) => { polls.push(h); return polls.length }) as unknown) as typeof setInterval
  globalThis.clearInterval = (() => {}) as typeof clearInterval
  let n = 0
  URL.createObjectURL = ((b: Blob) => { created.push(b); return `blob:test-${++n}` }) as typeof URL.createObjectURL
  URL.revokeObjectURL = ((u: string) => { revoked.push(u) }) as typeof URL.revokeObjectURL
})

afterEach(async () => {
  await unmountAll()
  fetched.length = 0
  revoked.length = 0
  created.length = 0
  polls.length = 0
  opened = null
})

after(async () => {
  globalThis.fetch = realFetch
  window.open = realOpen
  globalThis.setInterval = realSetInterval
  globalThis.clearInterval = realClearInterval
  URL.createObjectURL = realCreate
  URL.revokeObjectURL = realRevoke
  resetStoreForTest()
  await teardownDom()
})

const bots = [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }]
const previewBtn = (name: string) => document.querySelector<HTMLButtonElement>(`button.outbox-preview-btn[aria-label*="${name}"]`)

test('.html／.htm 與 .txt 有 ↗，.svg 與未列副檔名沒有；title 相同（不再標沙盒）', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('button.outbox-file').length === 5, '清單載入')
  assert.ok(previewBtn('chart.html'))
  assert.ok(previewBtn('old.htm'))
  assert.ok(previewBtn('note.txt'))
  assert.equal(previewBtn('logo.svg'), null)
  assert.equal(previewBtn('data.xyz'), null)
  assert.equal(previewBtn('chart.html')!.title, '開新分頁預覽（不下載）')
})

test('點 .html 的 ↗：產生 type=text/html 的 blob、新分頁（opener=null）直接導到它，不是 iframe；分頁關閉後 revoke', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('button.outbox-file').length === 5, '清單載入')
  await click(previewBtn('chart.html')!)
  await until(() => opened !== null && opened.href !== '', '新分頁導向')
  await settle()
  const o = opened!
  assert.equal(o.opener, null, 'w.opener 被切斷')
  const html = created.find((b) => b.type.startsWith('text/html'))
  assert.ok(html, '有一個 text/html 的 blob')
  assert.equal(html!.type, 'text/html;charset=utf-8')
  assert.equal(await html!.text(), HTML, '內容原封不動（含外部 script），不包殼、不加 CSP')
  assert.ok(o.href.startsWith('blob:test-'), `分頁導到 blob URL：${o.href}`)
  // 走帶 token 的非 inline 下載（後端回 octet-stream，type 由前端決定）。
  const fileReq = fetched.find((u) => u.includes('/outbox/file'))!
  assert.ok(fileReq.includes('chart.html') && !fileReq.includes('inline=1'))
  assert.ok(revoked.length >= 1, '原本取檔的 blob URL 已釋放')
  assert.ok(!revoked.includes(o.href), '分頁還開著，預覽用的 blob URL 不能先被收掉')
  // 分頁關閉後下一次輪詢才釋放預覽用的 blob URL。
  polls.forEach((p) => p())
  assert.ok(!revoked.includes(o.href), '分頁還開著：輪詢不收')
  o.closed = true
  polls.forEach((p) => p())
  assert.ok(revoked.includes(o.href), '分頁關閉後釋放')
})

test('其他副檔名（.txt）行為不變：走 inline 下載，不重包成 text/html', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('button.outbox-file').length === 5, '清單載入')
  await click(previewBtn('note.txt')!)
  await until(() => opened !== null && opened.href !== '', '新分頁導向')
  await settle()
  assert.ok(fetched.some((u) => u.includes('/outbox/file') && u.includes('inline=1')), 'txt 走 inline=1')
  assert.ok(!created.some((b) => b.type.startsWith('text/html')), '沒有 text/html 的 blob')
})
