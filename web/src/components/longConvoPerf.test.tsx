/**
 * 長對話的渲染成本（一顆 bot 幾百則訊息、含大量 code block）：用可數的量證明，不用掛鐘時間。
 * - 每個 store 更新時，掛在清單上的 selector 讀了幾次訊息陣列（O(N²) 的 `倒回` 計數器）。
 * - 同一份 Markdown 在清單重掛（換 bot 再換回來）時不重新解析。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Message, Run } from '../api/types'

before(setupDom)
after(teardownDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})

const { Bubble } = await import('./ChatPanel')
const { clearMarkdownCache, markdownCacheStats, MARKDOWN_CACHE_MAX_ENTRIES } = await import('../lib/markdownCache')

const bot = { id: 'b1', name: 'b1', kind: 'claude', project_id: 'p1', herdr_session: 'agents-manager' } as Bot
const code = '```ts\n' + 'const x = 1\n'.repeat(30) + '```\n'
const msg = (i: number, role: 'user' | 'assistant' = i % 2 ? 'assistant' : 'user'): Message =>
  ({
    id: `m${i}`, conversation_id: 'c', turn_id: `t${i >> 1}`, bot_id: 'b1', role,
    content: role === 'assistant' ? `回覆 ${i}\n\n${code}\n- a\n- b\n` : `問題 ${i}`,
    source: role === 'assistant' ? 'hook' : 'web', incomplete: false, group_id: null, attachments: [], relay_from: null,
    terminal_snapshot: null, created_at: new Date(1_800_000_000_000 + i * 1000).toISOString(),
  }) as Message

test('每次 store 更新，清單上的 selector 讀訊息陣列的次數跟則數成正比，不是平方（倒回計數器要等按下才算）', async () => {
  const N = 120
  const list = Array.from({ length: N }, (_, i) => msg(i))
  let reads = 0
  const counted = new Proxy(list, {
    get(target, prop, receiver) {
      if (typeof prop === 'string' && /^\d+$/.test(prop)) reads++
      return Reflect.get(target, prop, receiver)
    },
  })
  useStore.setState({
    bots: [bot],
    runs: { b1: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' } as unknown as Run },
    messages: { b1: counted },
  })
  await mount(<div>{list.map((m) => <Bubble key={m.id} msg={m} />)}</div>)
  assert.ok(document.querySelectorAll('.msg-rewind').length > 0, '倒回按鈕有畫出來（測試有效）')
  reads = 0
  for (let i = 1; i <= 10; i++) await act(async () => { useStore.setState({ lastSeq: i }) })
  assert.ok(reads <= 10 * N, `10 次無關更新讀了 ${reads} 次訊息陣列（上限 ${10 * N}）：每顆泡泡的 selector 不能各掃一遍清單`)
})

test('倒回的確認框在按下去的當下才數「之後還有幾則」', async () => {
  const list = Array.from({ length: 6 }, (_, i) => msg(i))
  useStore.setState({
    bots: [bot],
    runs: { b1: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' } as unknown as Run },
    messages: { b1: list },
  })
  await mount(<Bubble msg={list[2]} />)
  const btn = document.querySelector<HTMLButtonElement>('.msg-rewind')!
  await act(async () => { btn.click() })
  assert.ok(document.body.textContent?.includes('與之後的 3 則'), document.body.textContent ?? '')
})

test('同一份 Markdown 清單重掛（換 bot 再換回來）不重新解析', async () => {
  clearMarkdownCache()
  const list = Array.from({ length: 40 }, (_, i) => msg(i))
  useStore.setState({ bots: [bot], messages: { b1: list } })
  const el = <div>{list.map((m) => <Bubble key={m.id} msg={m} />)}</div>
  await mount(el)
  const first = markdownCacheStats().parses
  assert.equal(first, 20, '20 則 assistant 各解析一次')
  await unmountAll()
  await mount(<div>{list.map((m) => <Bubble key={m.id} msg={{ ...m }} />)}</div>)
  assert.equal(markdownCacheStats().parses, first, '重掛時（連物件都是新的）一則都不該重新解析')
  assert.ok(document.querySelectorAll('.bubble.md pre').length >= 20, '內容照樣畫出來')
})

test('Markdown 快取有上限：超過則數就丟最久沒用的，丟掉的下次再解析', async () => {
  clearMarkdownCache()
  const { renderMarkdown } = await import('../lib/markdownCache')
  for (let i = 0; i < MARKDOWN_CACHE_MAX_ENTRIES + 25; i++) renderMarkdown('b1', `# 標題 ${i}`)
  assert.ok(markdownCacheStats().size <= MARKDOWN_CACHE_MAX_ENTRIES)
  const before = markdownCacheStats().parses
  renderMarkdown('b1', `# 標題 ${MARKDOWN_CACHE_MAX_ENTRIES + 24}`)
  assert.equal(markdownCacheStats().parses, before, '最近的還在')
  renderMarkdown('b1', '# 標題 0')
  assert.equal(markdownCacheStats().parses, before + 1, '最舊的被丟掉了')
})

test('不同 bot 的同一段文字不共用（圖片元件綁 bot id）', async () => {
  clearMarkdownCache()
  const { renderMarkdown } = await import('../lib/markdownCache')
  renderMarkdown('b1', '![x](a.png)')
  const n = markdownCacheStats().parses
  renderMarkdown('b2', '![x](a.png)')
  assert.equal(markdownCacheStats().parses, n + 1)
})
