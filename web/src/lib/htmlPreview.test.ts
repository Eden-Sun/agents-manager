import test from 'node:test'
import assert from 'node:assert/strict'
import { isHtmlFile } from './htmlPreview'

test('只有 .html／.htm（不分大小寫）算 html 預覽；svg 與其他副檔名不算', () => {
  for (const n of ['a.html', 'B.HTML', 'x/y.htm', 'r.Htm']) assert.equal(isHtmlFile(n), true, n)
  for (const n of ['a.svg', 'a.txt', 'a.html.bak', 'html', 'a.xhtml']) assert.equal(isHtmlFile(n), false, n)
})
