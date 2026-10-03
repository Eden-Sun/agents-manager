/**
 * 分享頁的 bot 回覆走跟主 UI 一樣的複雜度上限（#797），不提供「仍用 Markdown 渲染」把公開頁卡住。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import ShareMarkdown from './ShareMarkdown'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

function clientWith(text: string): ShareClient {
  return {
    async messages() {
      return {
        bot_name: 'b',
        status: 'idle',
        has_more: false,
        messages: [{ id: 'a', role: 'assistant', text, created_at: '2024-01-01T00:00:00Z', attachments: [] }],
      }
    },
    async send() {},
    async upload() {
      return { id: 'u', name: 'u' }
    },
    async files() {
      return []
    },
    fileUrl: () => '/s/t/api/files/u',
    subscribe: () => () => {},
  }
}

test('超過 2 萬字的強調記號先顯示純文字，不跑 Markdown', async () => {
  const text = '**b** '.repeat(5000)
  assert.ok(text.length > 20_000)
  const root = await mount(<ShareMarkdown text={text} />)
  assert.equal(root.querySelectorAll('strong').length, 0)
  assert.ok(root.querySelector('pre'))
  assert.equal([...root.querySelectorAll('button')].some((b) => /Markdown/.test(b.textContent ?? '')), false)
})

test('引用超過 20 層顯示純文字', async () => {
  const text = '> '.repeat(21) + 'deep'
  const root = await mount(<ShareMarkdown text={text} />)
  assert.equal(root.querySelectorAll('blockquote').length, 0)
  assert.match(root.textContent ?? '', /deep/)
})

test('javascript:、data: 與協定相對連結不能變成可點的 href', async () => {
  for (const text of ['[點](javascript:alert(1))', '[點](data:text/html,x)', '[點](//evil.example/x)', '[點](https://evil.example/x)']) {
    const root = await mount(<ShareMarkdown text={text} />)
    const href = root.querySelector('a')?.getAttribute('href') ?? ''
    if (text.includes('https://evil')) assert.equal(href, 'https://evil.example/x')
    else assert.equal(root.querySelector('a'), null, text)
  }
})

test('短的 GFM 仍渲染表格、程式碼與連結', async () => {
  const root = await mount(<ShareMarkdown text={'| a | b |\n| - | - |\n| 1 | 2 |\n\n`code` 與 [連結](https://example.com)'} />)
  assert.ok(root.querySelector('table'))
  assert.ok(root.querySelector('code'))
  const a = root.querySelector('a')!
  assert.equal(a.getAttribute('rel')?.includes('noreferrer'), true)
})

test('單則轉換爆掉只退回那則，分享頁還在', async () => {
  const orig = console.error
  console.error = () => {}
  try {
    await mount(<ShareApp client={clientWith('__share_md_boom__')} />)
    await new Promise((r) => setTimeout(r, 30))
    assert.match(document.body.textContent!, /無法轉成 Markdown|__share_md_boom__/)
    assert.ok(document.querySelector('.sh-app'), '整頁不能被拆掉')
    assert.ok(document.querySelector('textarea'))
  } finally {
    console.error = orig
  }
})
