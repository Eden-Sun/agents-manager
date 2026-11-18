import test from 'node:test'
import assert from 'node:assert/strict'
import { INLINE_PREVIEW_EXTENSIONS } from './outboxList'
import { isSandboxPreviewable, renderSandboxShell, SANDBOX_FLAGS, SHELL_CSP, SHELL_HTML } from './sandboxPreview'

test('isSandboxPreviewable：只有 .html／.htm，不含 svg 與其他', () => {
  for (const n of ['a.html', 'dir/b.HTM', 'x.Html']) assert.equal(isSandboxPreviewable(n), true, n)
  for (const n of ['a.svg', 'a.txt', 'a.html.txt', 'html', 'a.xhtml', 'a.js']) assert.equal(isSandboxPreviewable(n), false, n)
})

test('INLINE_PREVIEW_EXTENSIONS 語意不變：沒有 html／htm／svg', () => {
  for (const e of ['html', 'htm', 'svg']) assert.ok(!INLINE_PREVIEW_EXTENSIONS.includes(e), e)
})

test('sandbox 旗標剛好是 allow-scripts', () => {
  assert.equal(SANDBOX_FLAGS, 'allow-scripts')
  assert.ok(!SANDBOX_FLAGS.includes('allow-same-origin'))
})

test('外殼 HTML 靜態：沒有 script、沒有 iframe、帶 CSP meta，CSP 不放行連網', () => {
  assert.ok(!/<script/i.test(SHELL_HTML))
  assert.ok(!/<iframe/i.test(SHELL_HTML))
  assert.ok(SHELL_HTML.includes('http-equiv="Content-Security-Policy"'))
  assert.match(SHELL_CSP, /default-src 'none'/)
  assert.match(SHELL_CSP, /connect-src 'none'/)
})

/** 最小的假 Document：記錄 write／createElement／srcdoc，驗證外殼是怎麼組出來的。 */
function fakeWindow() {
  const written: string[] = []
  const body = { children: [] as FakeEl[], appendChild(el: FakeEl) { this.children.push(el) } }
  class FakeEl {
    attrs: Record<string, string> = {}
    srcdoc = ''
    tag: string
    constructor(tag: string) {
      this.tag = tag
    }
    setAttribute(k: string, v: string) { this.attrs[k] = v }
  }
  const doc = {
    title: '',
    body,
    open() {},
    close() {},
    write(s: string) { written.push(s) },
    createElement(tag: string) { return new FakeEl(tag) },
  }
  return { w: { document: doc } as unknown as Window, written, body, doc }
}

test('renderSandboxShell：標題是檔名、iframe sandbox 剛好 allow-scripts、srcdoc 由 DOM 設定且不進外殼 HTML', () => {
  const { w, written, body, doc } = fakeWindow()
  const evil = '</body><script>top.location="x"</script>"><b>'
  renderSandboxShell(w, 'report.html', evil)
  assert.equal(doc.title, 'report.html')
  assert.deepEqual(written, [SHELL_HTML], '寫進文件的只有靜態外殼')
  assert.ok(!written[0].includes('top.location'))
  assert.equal(body.children.length, 1)
  const frame = body.children[0] as unknown as { tag: string; attrs: Record<string, string>; srcdoc: string }
  assert.equal(frame.tag, 'iframe')
  assert.equal(frame.attrs.sandbox, 'allow-scripts')
  assert.ok(!('allow-same-origin' in frame.attrs) && !frame.attrs.sandbox.includes('allow-same-origin'))
  assert.equal(frame.srcdoc, evil, 'bot 的 html 原樣放進 srcdoc')
})
