import test from 'node:test'
import assert from 'node:assert/strict'
import { eventIsFromCurrentTarget, eventTargetIsInsideCurrentTarget } from './domEvents.ts'

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
