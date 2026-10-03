import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { lazy, type ComponentType } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { MarkdownBoundary, MarkdownSuspense } from './SafeMarkdown'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('Markdown lazy chunk 等待時保留整段原文，載入後再換成格式化內容', async () => {
  let resolve!: (value: { default: ComponentType }) => void
  const PendingMarkdown = lazy(() => new Promise<{ default: ComponentType }>((done) => { resolve = done }))
  const text = '# 使用者輸入\n\n**不要消失** 與 [連結](https://example.test)'
  const root = await mount(
    <MarkdownBoundary plain={<pre className="md-plain">格式錯誤 fallback</pre>}>
      <MarkdownSuspense text={text}>
        <PendingMarkdown />
      </MarkdownSuspense>
    </MarkdownBoundary>,
  )

  assert.equal(root.querySelector('pre.md-plain')?.textContent, text, 'chunk 還沒到時也要完整顯示原文')
  await act(async () => resolve({ default: () => <p>已格式化</p> }))
  assert.equal(root.querySelector('p')?.textContent, '已格式化', 'chunk 到達後恢復 Markdown 樹')
})

test('Markdown lazy chunk 載入失敗由外層 boundary 接住並顯示原文', async () => {
  const originalError = console.error
  console.error = () => {}
  try {
    const FailedMarkdown = lazy(async () => {
      throw new Error('chunk unavailable')
    })
    const text = '這段訊息不能因為 chunk 載入失敗而消失'
    const root = await mount(
      <MarkdownBoundary plain={<pre className="md-plain">{text}</pre>}>
        <MarkdownSuspense text={text}>
          <FailedMarkdown />
        </MarkdownSuspense>
      </MarkdownBoundary>,
    )

    assert.equal(root.querySelector('pre.md-plain')?.textContent, text)
  } finally {
    console.error = originalError
  }
})
