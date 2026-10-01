import test from 'node:test'
import assert from 'node:assert/strict'
import { eventIsFromCurrentTarget, eventTargetIsInsideCurrentTarget, keyBelongsToControl } from './domEvents.ts'

test('row keyboard actions only handle keys dispatched on the row itself', () => {
  const row = {} as EventTarget
  const childButton = {} as EventTarget

  assert.equal(eventIsFromCurrentTarget({ target: row, currentTarget: row }), true)
  assert.equal(eventIsFromCurrentTarget({ target: childButton, currentTarget: row }), false)
})

test('row clicks ignore portal targets but keep clicks on DOM descendants', () => {
  const row = {} as Node
  const childText = {} as Node
  const portalButton = {} as Node
  const element = {
    contains: (target: Node) => target === row || target === childText,
  } as unknown as Element

  assert.equal(eventTargetIsInsideCurrentTarget({ target: row, currentTarget: element }), true)
  assert.equal(eventTargetIsInsideCurrentTarget({ target: childText, currentTarget: element }), true)
  assert.equal(eventTargetIsInsideCurrentTarget({ target: portalButton, currentTarget: element }), false)
  assert.equal(eventTargetIsInsideCurrentTarget({ target: null, currentTarget: element }), false)
})

test('keyboard shortcuts of a container leave the keys of focused buttons / checkboxes / links alone', () => {
  const el = (tagName: string, type?: string) => ({ tagName, type }) as unknown as EventTarget
  assert.equal(keyBelongsToControl(el('BUTTON')), true)
  assert.equal(keyBelongsToControl(el('A')), true)
  assert.equal(keyBelongsToControl(el('SELECT')), true)
  assert.equal(keyBelongsToControl(el('TEXTAREA')), true)
  assert.equal(keyBelongsToControl(el('INPUT', 'checkbox')), true)
  assert.equal(keyBelongsToControl(el('INPUT', 'radio')), true)
  // 文字輸入框（篩選格）與容器本身仍歸容器的快捷鍵管。
  assert.equal(keyBelongsToControl(el('INPUT', 'text')), false)
  assert.equal(keyBelongsToControl(el('DIV')), false)
  assert.equal(keyBelongsToControl(null), false)
})
