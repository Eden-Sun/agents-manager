/**
 * outbox 的 .html 沙盒預覽（使用者 2026-11-18）：.html／.htm 出現 ↗，其他未列副檔名（.svg／.xyz）不出現；
 * 點 ↗ 走「帶 token 下載取文字 → 空白分頁裡建沙盒外殼」，iframe 的 sandbox 剛好是 allow-scripts、srcdoc 由 DOM 設定。
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
const HTML = '<h1>hi</h1><script>document.title="x"</script></body><b>'
const fetched: string[] = []
const revoked: string[] = []

interface FakeFrame { tag: string; attrs: Record<string, string>; srcdoc: string }
let opened: { opener: unknown; written: string[]; title: string; frames: FakeFrame[] } | null = null

before(async () => {
  await setupDom()
  realOpen = window.open
  window.open = (() => {
    const state = { opener: window as unknown, written: [] as string[], title: '', frames: [] as FakeFrame[] }
    opened = state
    const doc = {
      open() {},
      close() {},
      write: (s: string) => state.written.push(s),
      get title() { return state.title },
      set title(v: string) { state.title = v },
      createElement: (tag: string) => ({ tag, attrs: {} as Record<string, string>, srcdoc: '', setAttribute(k: string, v: string) { this.attrs[k] = v } }),
      body: { appendChild: (el: FakeFrame) => state.frames.push(el) },
    }
    return {
      closed: false,
      close() {},
      get opener() { return state.opener },
      set opener(v: unknown) { state.opener = v },
      location: { href: '' },
      document: doc,
    } as unknown as Window
  }) as typeof window.open
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    fetched.push(url)
    if (url.startsWith('blob:')) return Promise.resolve(new Response(HTML, { status: 200 }))
    if (url.includes('/outbox/file')) return Promise.resolve(new Response(HTML, { status: 200, headers: { 'content-type': 'application/octet-stream' } }))
    return Promise.resolve(json({
      dir: '/x', reason: null, ttl_secs: 3600, kept: false,
      files: ['chart.html', 'old.htm', 'note.txt', 'logo.svg', 'data.xyz'].map((name) => ({ name, size: 3, modified: 1, remaining_secs: 1800 })),
    }))
  }) as typeof fetch
  URL.createObjectURL = (() => 'blob:test') as typeof URL.createObjectURL
  URL.revokeObjectURL = ((u: string) => { revoked.push(u) }) as typeof URL.revokeObjectURL
})

afterEach(async () => {
  await unmountAll()
  fetched.length = 0
  revoked.length = 0
  opened = null
})

after(async () => {
  globalThis.fetch = realFetch
  window.open = realOpen
  URL.createObjectURL = realCreate
  URL.revokeObjectURL = realRevoke
  resetStoreForTest()
  await teardownDom()
})

const bots = [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }]
const previewBtn = (name: string) => document.querySelector<HTMLButtonElement>(`button.outbox-preview-btn[aria-label*="${name}"]`)

test('.html／.htm 與 .txt 有 ↗，.svg 與未列副檔名沒有；html 的 title 標示沙盒', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('button.outbox-file').length === 5, '清單載入')
  assert.ok(previewBtn('chart.html'))
  assert.ok(previewBtn('old.htm'))
  assert.ok(previewBtn('note.txt'))
  assert.equal(previewBtn('logo.svg'), null)
  assert.equal(previewBtn('data.xyz'), null)
  assert.equal(previewBtn('chart.html')!.title, '開新分頁預覽（沙盒，不下載）')
  assert.equal(previewBtn('note.txt')!.title, '開新分頁預覽（不下載）')
})

test('點 .html 的 ↗：外殼只含靜態內容，iframe sandbox 剛好 allow-scripts、srcdoc 由 DOM 設定；opener 被切斷', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('button.outbox-file').length === 5, '清單載入')
  await click(previewBtn('chart.html')!)
  await until(() => opened !== null && opened.frames.length === 1, '外殼建好')
  await settle()
  const o = opened!
  assert.equal(o.opener, null, 'w.opener 被切斷')
  assert.equal(o.title, 'chart.html')
  assert.equal(o.written.length, 1)
  assert.ok(!o.written[0].includes('<script') && !o.written[0].includes('<h1>'), '外殼 HTML 不含 bot 內容或 script')
  const frame = o.frames[0]
  assert.equal(frame.tag, 'iframe')
  assert.equal(frame.attrs.sandbox, 'allow-scripts')
  assert.ok(!frame.attrs.sandbox.includes('allow-same-origin'))
  assert.equal(frame.srcdoc, HTML)
  // 後端回的是 octet-stream、走帶 token 的非 inline 下載（沒有 inline=1）。
  const fileReq = fetched.find((u) => u.includes('/outbox/file'))!
  assert.ok(fetched.includes('blob:test'), '文字從帶 token 取回的 blob 讀')
  assert.ok(fileReq.includes('chart.html') && !fileReq.includes('inline=1'))
  assert.ok(revoked.length >= 1, '取完文字就釋放 blob URL')
})
