/**
 * bot 的輸出是不可信的內容（可能被 prompt injection）：Markdown 呈現不能讓它執行程式、把整個 app 換頁、或把畫面卡死。
 * - 協定：javascript:／data:／vbscript: 不能成為 href／src，原始 HTML 不能變成元素（react-markdown 預設擋，這裡釘住）。
 * - 連結一律新分頁＋noopener noreferrer：不然 bot 給的連結一按就整個 app 換頁（連同記憶體裡的 token）。
 * - 太長的訊息（remark 的某些輸入是平方時間）預設純文字，使用者按了才用 Markdown 展開。
 * - 巢狀太深（`> > > …`）：預設純文字；轉換真的丟了例外也由 error boundary 接住（整個 app 沒有 error boundary，沒接住就白畫面）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { MarkdownBoundary, SafeMarkdown } from './SafeMarkdown'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const DANGEROUS = /^\s*(javascript|data|vbscript):/i

async function render(md: string, botId: string | null = 'b1'): Promise<HTMLElement> {
  return mount(<SafeMarkdown text={md} botId={botId} />)
}

test('危險協定與原始 HTML 不會變成可執行的元素或屬性', async () => {
  const payloads = [
    '[x](javascript:alert(1))',
    '[x](JaVaScRiPt:alert(1))',
    '[x](  javascript:alert(1))',
    '[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)',
    '[x](vbscript:msgbox(1))',
    '[x](javascript&colon;alert(1))',
    '[x](&#106;avascript:alert(1))',
    '[x][r]\n\n[r]: javascript:alert(1)',
    '<javascript:alert(1)>',
    '<a href="javascript:alert(1)">x</a>',
    '<img src=x onerror=alert(1)>',
    '<script>alert(1)</script>',
    '<iframe src="javascript:alert(1)"></iframe>',
    '<svg onload=alert(1)>',
    '![x](javascript:alert(1))',
    '![x](data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=)',
  ]
  for (const md of payloads) {
    const root = await render(md)
    for (const el of root.querySelectorAll('*')) {
      assert.ok(!['SCRIPT', 'IFRAME', 'SVG', 'STYLE', 'OBJECT', 'EMBED'].includes(el.tagName.toUpperCase()), `${md} → <${el.tagName}>`)
      for (const attr of el.getAttributeNames()) {
        assert.ok(!attr.startsWith('on'), `${md} → ${attr}`)
        if (attr === 'href' || attr === 'src') assert.ok(!DANGEROUS.test(el.getAttribute(attr) ?? ''), `${md} → ${attr}=${el.getAttribute(attr)}`)
      }
    }
    await unmountAll()
  }
})

test('連結一律新分頁、noopener noreferrer：bot 給的連結不能把整個 app 換頁，也不帶 opener／referrer', async () => {
  const root = await render('看 [這裡](https://example.com/a) 與 <https://example.org> 與 https://example.net/auto')
  const links = [...root.querySelectorAll('a')]
  assert.equal(links.length, 3)
  for (const a of links) {
    assert.equal(a.getAttribute('target'), '_blank', a.outerHTML)
    const rel = (a.getAttribute('rel') ?? '').split(/\s+/)
    assert.ok(rel.includes('noopener') && rel.includes('noreferrer'), a.outerHTML)
  }
})

test('被清掉網址的連結不留一個點了沒反應的 <a>，字還在', async () => {
  const root = await render('[點我](javascript:alert(1))')
  assert.equal(root.querySelectorAll('a').length, 0)
  assert.match(root.textContent ?? '', /點我/)
})

test('太長的訊息預設純文字（不跑 Markdown），按了才展開', async () => {
  const md = '**b** '.repeat(5000)
  assert.ok(md.length > 20_000)
  const root = await render(md)
  assert.equal(root.querySelectorAll('strong').length, 0, '太長不該跑 Markdown')
  assert.ok(root.querySelector('pre.md-plain'), '要有純文字版')
  assert.ok((root.textContent ?? '').includes('**b** **b**'), '字原樣在')
  const btn = [...root.querySelectorAll('button')].find((b) => /Markdown/.test(b.textContent ?? ''))
  assert.ok(btn, '要有「以 Markdown 顯示」')
  await click(btn!)
  assert.ok(root.querySelectorAll('strong').length > 0, '按了之後才跑 Markdown')
})

test('一般長度照常渲染 Markdown', async () => {
  const root = await render('# 標題\n\n**粗** 與 `code`')
  assert.ok(root.querySelector('h1'))
  assert.ok(root.querySelector('strong'))
  assert.equal(root.querySelector('pre.md-plain'), null)
})

test('巢狀深得離譜（引用 500 層、清單縮排 200 格）預設純文字：那種輸入轉換遞迴爆 stack、或慢到分鐘級', async () => {
  for (const md of ['> '.repeat(500) + 'deep', Array.from({ length: 5 }, (_, i) => ' '.repeat(i * 50) + '- x').join('\n')]) {
    const root = await render(md)
    assert.ok(root.querySelector('pre.md-plain'), md.slice(0, 20))
    assert.equal(root.querySelectorAll('blockquote, li').length, 0, '不該跑 Markdown 轉換')
    assert.ok([...root.querySelectorAll('button')].some((b) => /Markdown/.test(b.textContent ?? '')))
    await unmountAll()
  }
})

test('正常的巢狀（引用 5 層、清單 6 層）照常渲染', async () => {
  const root = await render('> '.repeat(5) + 'q\n\n' + Array.from({ length: 6 }, (_, i) => '  '.repeat(i) + '- x').join('\n'))
  assert.ok(root.querySelector('blockquote'))
  assert.ok(root.querySelector('li'))
  assert.equal(root.querySelector('pre.md-plain'), null)
})

test('轉換中丟了例外：error boundary 接住、退回純文字，不是整個 app 白畫面', async () => {
  const origError = console.error
  console.error = () => {} // React 會把被接住的例外印出來
  try {
    function Boom(): never {
      throw new RangeError('Maximum call stack size exceeded')
    }
    const root = await mount(
      <MarkdownBoundary plain={<pre className="md-plain">原文</pre>}>
        <Boom />
      </MarkdownBoundary>,
    )
    assert.equal(root.querySelector('pre.md-plain')?.textContent, '原文')
  } finally {
    console.error = origError
  }
})
