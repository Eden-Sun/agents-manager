import test from 'node:test'
import assert from 'node:assert/strict'
import { safeHttpUrl } from './safeUrl.ts'

test('http／https 網址原樣放行', () => {
  assert.equal(safeHttpUrl('https://github.com/a/b/issues/1'), 'https://github.com/a/b/issues/1')
  assert.equal(safeHttpUrl('http://127.0.0.1:5173/x?y=1#z'), 'http://127.0.0.1:5173/x?y=1#z')
})

test('javascript:／data:／vbscript:／file: 與看不懂的一律不給（連大小寫、前導空白、控制字元的變形也不行）', () => {
  for (const bad of [
    'javascript:alert(1)',
    'JaVaScRiPt:alert(1)',
    '  javascript:alert(1)',
    '\tjava\nscript:alert(1)',
    'data:text/html;base64,PHNjcmlwdD4=',
    'vbscript:msgbox(1)',
    'file:///etc/passwd',
    '//evil.example/x',
    '/relative/path',
    'not a url',
    '',
  ]) {
    assert.equal(safeHttpUrl(bad), undefined, JSON.stringify(bad))
  }
  assert.equal(safeHttpUrl(null), undefined)
  assert.equal(safeHttpUrl(undefined), undefined)
})
