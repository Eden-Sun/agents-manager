import test from 'node:test'
import assert from 'node:assert/strict'
import { openTerminalLink } from './terminalLink'

function stubWindow(): { opened: unknown[][]; confirms: number; restore: () => void } {
  const prev = (globalThis as { window?: unknown }).window
  const state = { opened: [] as unknown[][], confirms: 0, restore: () => {} }
  ;(globalThis as { window?: unknown }).window = {
    open: (...args: unknown[]) => { state.opened.push(args) },
    confirm: () => { state.confirms++; return true },
  }
  state.restore = () => { (globalThis as { window?: unknown }).window = prev }
  return state
}

test('https 連結直接開新分頁，不跳確認框', () => {
  const w = stubWindow()
  try {
    openTerminalLink('https://accounts.google.com/o/oauth2/auth?x=1')
    assert.deepEqual(w.opened, [['https://accounts.google.com/o/oauth2/auth?x=1', '_blank', 'noopener,noreferrer']])
    assert.equal(w.confirms, 0)
  } finally {
    w.restore()
  }
})

test('http 連結也直接開，非 http(s) 的 scheme 一律忽略', () => {
  const w = stubWindow()
  try {
    openTerminalLink('http://localhost:7788/')
    for (const uri of ['javascript:alert(1)', 'file:///etc/passwd', 'data:text/html,x', 'not a url', '']) {
      openTerminalLink(uri)
    }
    assert.equal(w.opened.length, 1)
    assert.equal(w.opened[0]?.[0], 'http://localhost:7788/')
    assert.equal(w.confirms, 0)
  } finally {
    w.restore()
  }
})
