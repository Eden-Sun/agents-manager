import test from 'node:test'
import assert from 'node:assert/strict'
import { isImeEnter } from './ime.ts'
import { herdrKeyFromEvent } from '../hooks/usePaneKeys.ts'

function keydown(overrides: Partial<KeyboardEvent> = {}): KeyboardEvent {
  return {
    key: 'Enter',
    keyCode: 13,
    isComposing: false,
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    ...overrides,
  } as KeyboardEvent
}

test('IME Enter is recognized while composition is active', () => {
  assert.equal(isImeEnter(keydown({ isComposing: true })), true)
})

test('Safari IME Enter is recognized after compositionend reports false', () => {
  assert.equal(isImeEnter(keydown({ keyCode: 229 })), true)
})

test('ordinary Enter is not classified as IME input', () => {
  assert.equal(isImeEnter(keydown()), false)
})

test('pane passthrough drops Safari IME Enter instead of sending it to herdr', () => {
  assert.equal(herdrKeyFromEvent(keydown({ keyCode: 229 })), null)
})
