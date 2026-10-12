import test from 'node:test'
import assert from 'node:assert/strict'
import { themeFromStorageEvent } from './theme.ts'

/** 只記屬性的假 html 元素。 */
function fakeRoot() {
  const attrs = new Map<string, string>()
  return {
    attrs,
    setAttribute: (k: string, v: string) => void attrs.set(k, v),
    removeAttribute: (k: string) => void attrs.delete(k),
  }
}

test('另一個分頁改了主題：這個分頁照那個值套上去（storage 事件只送給其他分頁）', () => {
  const root = fakeRoot()
  assert.equal(themeFromStorageEvent('am-theme', 'dark', root), 'dark')
  assert.equal(root.attrs.get('data-theme'), 'dark')
  assert.equal(themeFromStorageEvent('am-theme', 'light', root), 'light')
  assert.equal(root.attrs.get('data-theme'), 'light')
  assert.equal(themeFromStorageEvent('am-theme', null, root), 'system')
  assert.equal(root.attrs.has('data-theme'), false)
})

test('不是主題的鍵不碰，回 null', () => {
  const root = fakeRoot()
  root.attrs.set('data-theme', 'dark')
  assert.equal(themeFromStorageEvent('am.selection', '{}', root), null)
  assert.equal(root.attrs.get('data-theme'), 'dark')
})
