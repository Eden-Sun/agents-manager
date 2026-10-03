import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { useState, type ComponentProps } from 'react'
import type { UrlTransform } from 'react-markdown'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { clearMarkdownCache, markdownCacheStats, renderMarkdown } from '../lib/markdownCache'
import { markdownComponents } from '../lib/markdownComponents'
import { SafeMarkdown } from './SafeMarkdown'

before(setupDom)
beforeEach(clearMarkdownCache)
afterEach(async () => {
  await unmountAll()
  clearMarkdownCache()
})
after(teardownDom)

test('手動展開的超長訊息仍不進 Markdown 快取', async () => {
  const text = '**字** '.repeat(4_200)
  assert.ok(text.length > 20_000 && text.length < 50_000)
  const root = await mount(<SafeMarkdown text={text} botId="b1" />)
  assert.equal(markdownCacheStats().size, 0, '預設純文字不能解析或快取')

  await click([...root.querySelectorAll('button')].find((b) => b.textContent?.includes('以 Markdown 顯示'))!)
  assert.ok(root.querySelector('strong'), '使用者要求展開時仍照常轉成 Markdown')
  assert.equal(markdownCacheStats().size, 0, '手動展開也不能把超長訊息留在快取')
})

test('手動展開的超深訊息仍不進 Markdown 快取', async () => {
  const text = `${'> '.repeat(21)}深層引用`
  const root = await mount(<SafeMarkdown text={text} botId="b1" />)
  assert.equal(markdownCacheStats().size, 0, '預設純文字不能解析或快取')

  await click([...root.querySelectorAll('button')].find((b) => b.textContent?.includes('以 Markdown 顯示'))!)
  assert.ok(root.querySelector('blockquote'), '使用者要求展開時仍照常轉成 Markdown')
  assert.equal(markdownCacheStats().size, 0, '手動展開也不能把超深訊息留在快取')
})

test('串流草稿 cache=false 時照常渲染但不留下快取項目', async () => {
  const root = await mount(<SafeMarkdown text="**正在輸出**" botId="b1" cache={false} />)
  assert.ok(root.querySelector('strong'))
  assert.equal(markdownCacheStats().size, 0)
})

test('快取樹的元件 render 出錯時 boundary 接住並淘汰壞樹；換 bot 後同文可恢復', async () => {
  const components = markdownComponents('b1')
  const originalLink = components.a
  components.a = (() => { throw new Error('renderer failed') }) as typeof originalLink
  let setBotId: (id: string) => void = () => {}
  function Probe() {
    const [botId, set] = useState('b1')
    setBotId = set
    return <SafeMarkdown text="[link](https://example.test)" botId={botId} />
  }
  const originalError = console.error
  console.error = () => {}
  try {
    const root = await mount(<Probe />)
    assert.ok(root.querySelector('.md-plain-note'), '快取路徑中的 renderer 例外要由 SafeMarkdown boundary 接住')
    assert.equal(markdownCacheStats().size, 0, '失敗的 render 樹不能留在快取')

    components.a = originalLink
    await act(async () => setBotId('b2'))
    assert.ok(root.querySelector('a'), 'bot id 變更後 boundary 不應沿用前一顆 bot 的失敗狀態')
  } finally {
    components.a = originalLink
    console.error = originalError
  }
})

test('同一文字與 bot 的元件或 URL transform 換代後會重新 parse，不回傳舊 React 樹', async () => {
  const base = markdownComponents('b1')
  const componentsA = { ...base, a: (props: ComponentProps<'a'>) => <a href={props.href} className="link-a">{props.children}</a> }
  const componentsB = { ...base, a: (props: ComponentProps<'a'>) => <a href={props.href} className="link-b">{props.children}</a> }
  const transformA: UrlTransform = (url) => url.replace('example.test', 'a.example.test')
  const transformB: UrlTransform = (url) => url.replace('example.test', 'b.example.test')
  const text = '[link](https://example.test)'

  const first = await mount(<>{renderMarkdown('b1', text, { components: componentsA, urlTransform: transformA })}</>)
  assert.equal(first.querySelector('a')?.className, 'link-a')
  assert.equal(first.querySelector('a')?.getAttribute('href'), 'https://a.example.test')
  await unmountAll()

  const componentChanged = await mount(<>{renderMarkdown('b1', text, { components: componentsB, urlTransform: transformA })}</>)
  assert.equal(componentChanged.querySelector('a')?.className, 'link-b', '元件替換不可命中舊樹')
  assert.equal(componentChanged.querySelector('a')?.getAttribute('href'), 'https://a.example.test')
  await unmountAll()

  const transformChanged = await mount(<>{renderMarkdown('b1', text, { components: componentsB, urlTransform: transformB })}</>)
  assert.equal(transformChanged.querySelector('a')?.className, 'link-b')
  assert.equal(transformChanged.querySelector('a')?.getAttribute('href'), 'https://b.example.test', 'URL policy 替換不可回傳舊 href')
})
