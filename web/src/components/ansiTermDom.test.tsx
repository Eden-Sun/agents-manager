/**
 * AnsiTerm 真的掛進 happy-dom：色段變成帶 class／color 的 span、網址仍可點（跨色段也行）、沒有原始 ESC、沒有 key 警告。
 */
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { AnsiTerm } from './AnsiTerm'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const E = '\x1b'

test('色段：每段一個 span，樣式與主題變數落在 inline style／class 上', async () => {
  const el = await mount(<pre className="term"><AnsiTerm text={`${E}[1;31mERR${E}[0m ok\n${E}[38;5;196mhot${E}[0m`} /></pre>)
  const spans = [...el.querySelectorAll('span.ansi-b')]
  assert.equal(spans.length, 1)
  assert.equal(spans[0].textContent, 'ERR')
  assert.equal((spans[0] as HTMLElement).style.color, 'var(--ansi-1)')
  const hot = [...el.querySelectorAll('span')].find((s) => s.textContent === 'hot') as HTMLElement
  assert.equal(hot.style.color, '#ff0000')
  assert.equal(el.textContent, 'ERR ok\nhot')
})

test('沒有原始跳脫字元或其他控制字元進到 DOM', async () => {
  const el = await mount(<pre><AnsiTerm text={`a${E}[2K${E}]0;t\x07b\rc${E}[?25h\x1b`} /></pre>)
  assert.equal(el.textContent, 'abc')
})

test('網址仍可點；網址跨色段時每一段都是連結，點了複製整條網址', async () => {
  const text = `go ${E}[33mhttps://example.com/pa${E}[0mth?x=1${E}[0m done`
  const el = await mount(<pre><AnsiTerm text={text} /></pre>)
  const links = [...el.querySelectorAll('button.term-link')] as HTMLButtonElement[]
  assert.deepEqual(links.map((b) => b.textContent), ['https://example.com/pa', 'th?x=1'])
  assert.ok(links.every((b) => b.title.includes('https://example.com/path?x=1')))
  assert.equal(el.textContent, 'go https://example.com/path?x=1 done')
})

test('OSC 8 超連結：顯示文字是連結', async () => {
  const el = await mount(<pre><AnsiTerm text={`${E}]8;;https://a.example/login${E}\\登入${E}]8;;${E}\\`} /></pre>)
  const btn = el.querySelector('button.term-link') as HTMLButtonElement
  assert.equal(btn.textContent, '登入')
  assert.ok(btn.title.includes('https://a.example/login'))
})

test('分隔線依 linkifyTerm 規則縮到 32 個', async () => {
  const rule = '─'.repeat(200)
  const el = await mount(<pre><AnsiTerm text={`${E}[2m${rule}${E}[0m\n${E}[90mx${E}[0m\n${rule}`} columns={213} /></pre>)
  assert.deepEqual(el.textContent!.split('\n'), ['─'.repeat(32), 'x', '─'.repeat(32)])
})

test('反白：沒有指定顏色時以主題的文字／背景對調', async () => {
  const el = await mount(<pre><AnsiTerm text={`${E}[7mhi${E}[0m`} /></pre>)
  const span = el.querySelector('span.ansi-term > span') as HTMLElement
  assert.equal(span.style.color, 'var(--term-bg)')
  assert.equal(span.style.backgroundColor, 'var(--term-text)')
})

test('大量色段重繪也不噴 React key 警告', async () => {
  const warnings: unknown[][] = []
  const orig = console.error
  console.error = (...args: unknown[]) => {
    warnings.push(args)
  }
  try {
    const text = Array.from({ length: 50 }, (_, i) => `${E}[3${i % 8}m${i} http://x.example/${i}${E}[0m`).join('\n')
    await act(async () => {
      await mount(<pre><AnsiTerm text={text} columns={80} /></pre>)
    })
  } finally {
    console.error = orig
  }
  assert.equal(warnings.filter((w) => String(w[0]).includes('key')).length, 0)
})
