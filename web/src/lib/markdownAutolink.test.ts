/**
 * 訊息泡泡的自動連結（remark-gfm 的 autolink literal）與終端的網址規則（`termPieces`，#1533）一致：
 * 網址後面緊接的全形標點不算進網址，點下去開的是網址本身，不是連同後面整句。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { unified } from 'unified'
import remarkParse from 'remark-parse'
import remarkGfm from 'remark-gfm'
import { remarkTrimAutolinkPunct } from './markdownAutolink.ts'

function linksOf(md: string, withTrim: boolean): string[] {
  let p = unified().use(remarkParse).use(remarkGfm)
  if (withTrim) p = p.use(remarkTrimAutolinkPunct)
  const tree = p.runSync(p.parse(md), md) as unknown as { children: unknown[] }
  const out: string[] = []
  const walk = (n: { type?: string; url?: string; children?: unknown[] }) => {
    if (n.type === 'link' && n.url) out.push(n.url)
    for (const c of n.children ?? []) walk(c as never)
  }
  walk(tree as never)
  return out
}


test('全形標點緊接在網址後面：自動連結不包含它（請開 https://…/login。）', () => {
  assert.deepEqual(linksOf('請開 https://example.com/login。', true), ['https://example.com/login'])
})

test('頓號、逗號、驚嘆號都不算網址的一部分', () => {
  assert.deepEqual(linksOf('網址：https://example.com/login，然後貼上', true), ['https://example.com/login'])
  assert.deepEqual(linksOf('https://example.com/a、https://example.com/b', true), ['https://example.com/a', 'https://example.com/b'])
  assert.deepEqual(linksOf('看 https://example.com/a！', true), ['https://example.com/a'])
})

test('網址中間的中文字不切（只切標點）', () => {
  assert.deepEqual(linksOf('https://example.com/中文路徑。', true), ['https://example.com/中文路徑'])
})

test('ASCII 的結尾標點照舊由 remark 處理，外掛不改它', () => {
  assert.deepEqual(linksOf('see (https://example.com/docs/page.) here', true), ['https://example.com/docs/page'])
})

test('句點跟全形標點連在一起：句點一起剝掉（`…/x.。` 與終端同一個結果）', () => {
  assert.deepEqual(linksOf('https://a.b/x.。好', true), ['https://a.b/x'])
})

test('同一段的第二個網址是大寫 scheme：照樣認得出來，不能變成不能點的文字', () => {
  assert.deepEqual(linksOf('https://a.com/x、HTTPS://b.com/y', true), ['https://a.com/x', 'HTTPS://b.com/y'])
})

test('同一段的第二個網址是 www.：照樣認得出來（連結目標是 http:// 加上 www.…）', () => {
  assert.deepEqual(linksOf('https://a.com/x，www.b.com/y', true), ['https://a.com/x', 'http://www.b.com/y'])
})

test('長訊息：很多網址緊接著全形標點（沒有空白）也要切得對、而且每一個都認得出來', () => {
  const md = Array.from({ length: 300 }, (_, i) => `https://example.com/p${i}，`).join('')
  const got = linksOf(md, true)
  assert.equal(got.length, 300)
  assert.equal(got[0], 'https://example.com/p0')
  assert.equal(got[299], 'https://example.com/p299')
})

test('ASCII 右括號不成對、後面接全形標點：括號不算網址（同終端；(https://…/a)。 的網址是 …/a）', () => {
  assert.deepEqual(linksOf('(https://example.com/a)。', true), ['https://example.com/a'])
  assert.deepEqual(linksOf('see (https://example.com/a)，好', true), ['https://example.com/a'])
  // 成對的括號是網址自己的，要留著。
  assert.deepEqual(linksOf('https://en.wikipedia.org/wiki/Foo_(bar)。', true), ['https://en.wikipedia.org/wiki/Foo_(bar)'])
})

test('明確寫成的 [文字](網址) 不動：文字與目標相同、含全形標點也照原樣（它不是自動連結）', () => {
  assert.deepEqual(linksOf('[https://example.com/x。](https://example.com/x。)', true), ['https://example.com/x。'])
})
