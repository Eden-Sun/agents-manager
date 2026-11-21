import test from 'node:test'
import assert from 'node:assert/strict'
import { isStaleBundle, loadedMainScript, mainScriptFromHtml, shouldCheck } from './staleBundle.ts'

const BUILT = `<!doctype html><html><head>
<link rel="stylesheet" href="/assets/index-OLD.css">
<script type="module" crossorigin src="/assets/index-AAA111.js"></script>
</head><body></body></html>`

test('從 index.html 取出正式 build 的主 script', () => {
  assert.equal(mainScriptFromHtml(BUILT), '/assets/index-AAA111.js')
  assert.equal(mainScriptFromHtml('<script src="/assets/x.js"></script>'), null, '沒有 type=module 不算')
  assert.equal(mainScriptFromHtml('<script type="module" src="/src/main.tsx"></script>'), null, 'dev 模式不算')
  assert.equal(mainScriptFromHtml('<script type=\'module\' src=\'/assets/index-B.js\'></script>'), '/assets/index-B.js', '單引號也認')
})

test('載入中的主 script：從 DOM 讀；dev 模式回 null', () => {
  const doc = { querySelector: (sel: string) => (sel.includes('/assets/') ? { getAttribute: () => '/assets/index-AAA111.js' } : null) }
  assert.equal(loadedMainScript(doc as unknown as Document), '/assets/index-AAA111.js')
  assert.equal(loadedMainScript({ querySelector: () => null } as unknown as Document), null)
})

test('相同不算換版；不同才算；任一邊拿不到都不算', () => {
  assert.equal(isStaleBundle('/assets/index-AAA111.js', '/assets/index-AAA111.js'), false, '相同')
  assert.equal(isStaleBundle('/assets/index-AAA111.js', '/assets/index-BBB222.js'), true, '不同＝換版')
  assert.equal(isStaleBundle(null, '/assets/index-BBB222.js'), false, 'dev 模式（載入的是 null）跳過')
  assert.equal(isStaleBundle('/assets/index-AAA111.js', null), false, '抓不到伺服器版本不算')
})

test('節流：30 秒內不重查，之後才查；第一次一定查', () => {
  assert.equal(shouldCheck(1_000, null), true, '第一次')
  assert.equal(shouldCheck(20_000, 1_000), false, '19 秒後不查')
  assert.equal(shouldCheck(31_000, 1_000), true, '30 秒後查')
  assert.equal(shouldCheck(5_000, 1_000, 10_000), false, '自訂間隔')
})
