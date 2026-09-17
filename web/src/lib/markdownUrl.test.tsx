import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import Markdown from 'react-markdown'
import { markdownUrlTransform, readableImagePath } from './markdownUrl.ts'

/** 照 ChatPanel 的方式渲染，收集 img 元件拿到的 src 與連結的 href。 */
function render(md: string): { srcs: string[]; html: string } {
  const srcs: string[] = []
  const html = renderToStaticMarkup(
    <Markdown
      urlTransform={markdownUrlTransform}
      components={{
        img: (p) => {
          srcs.push(String(p.src))
          return null
        },
      }}
    >
      {md}
    </Markdown>,
  )
  return { srcs, html }
}

test('file:// 圖片的 src 原樣交給元件，不被清成空字串（review3 c4 L3）', () => {
  assert.deepEqual(render('![shot](file:///Users/me/proj/docs/a.png)').srcs, ['file:///Users/me/proj/docs/a.png'])
  // 相對路徑、網址照舊。
  assert.deepEqual(render('![a](docs/a.png)').srcs, ['docs/a.png'])
  assert.deepEqual(render('![a](https://example.com/a.png)').srcs, ['https://example.com/a.png'])
})

test('只放行圖片：連結的 file:／javascript: 照預設規則清掉', () => {
  assert.doesNotMatch(render('[x](file:///etc/passwd)').html, /file:/)
  assert.doesNotMatch(render('[x](javascript:alert(1))').html, /javascript:/)
  assert.deepEqual(render('![x](javascript:alert(1))').srcs, [''])
})

test('讀不到時寫給人看的路徑解回原字', () => {
  assert.equal(readableImagePath('docs/%E6%88%AA%E5%9C%96.png'), 'docs/截圖.png')
  assert.equal(readableImagePath('docs/my%20shot.png'), 'docs/my shot.png')
  assert.equal(readableImagePath('docs/100%.png'), 'docs/100%.png', '解不開就原樣')
})
