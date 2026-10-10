import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act } from 'react'
import { Modal } from './Modal'
import { keydown, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

test('組字中的 Esc 不關 Modal', async () => {
  let closed = 0
  await mount(
    <Modal open title="t" onClose={() => { closed += 1 }}>
      <input aria-label="x" />
    </Modal>,
  )
  const input = document.querySelector('input')!
  await keydown(input, 'Escape', { isComposing: true })
  assert.equal(closed, 0)
  assert.ok(document.querySelector('.modal'))
})

test('keyCode 229 的 Esc 也不關', async () => {
  let closed = 0
  await mount(
    <Modal open title="t" onClose={() => { closed += 1 }}>
      <input aria-label="x" />
    </Modal>,
  )
  const input = document.querySelector('input')!
  const ev = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })
  Object.defineProperty(ev, 'keyCode', { value: 229 })
  await act(async () => {
    input.dispatchEvent(ev)
  })
  assert.equal(closed, 0)
  assert.ok(document.querySelector('.modal'))
})

test('一般的 Esc 照舊關', async () => {
  let closed = 0
  await mount(
    <Modal open title="t" onClose={() => { closed += 1 }}>
      <input aria-label="x" />
    </Modal>,
  )
  const input = document.querySelector('input')!
  await keydown(input, 'Escape')
  assert.equal(closed, 1)
})
