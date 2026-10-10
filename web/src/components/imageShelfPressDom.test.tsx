import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll, settle } from '../testing/domHarness'
import { ImageShelf } from './ImageShelf'
import { useShelf } from '../store/shelf'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  useShelf.getState().clear()
  useShelf.getState().setSink(null)
})
after(teardownDom)

const ptr = (type: string, x = 10, y = 10, pointerId = 1) =>
  new PointerEvent(type, { bubbles: true, clientX: x, clientY: y, pointerType: 'touch', button: 0, pointerId })

test('長按後沒有 click（pointercancel）：下一次點擊要能收起預覽', async () => {
  useShelf.getState().add([new File([new Uint8Array([137, 80, 78, 71])], 'a.png', { type: 'image/png' })])
  const handed: File[][] = []
  useShelf.getState().setSink({ add: (f) => handed.push(f), label: 'x' })

  await mount(<ImageShelf />)
  const card = document.querySelector<HTMLButtonElement>('.shelf-card-main')!
  assert.ok(card, 'card must exist')

  await act(async () => {
    card.dispatchEvent(ptr('pointerdown'))
    await settle(500)
  })
  assert.ok(document.querySelector('.shelf-peek'), '長按 450ms 後預覽要打開')

  await act(async () => {
    card.dispatchEvent(ptr('pointercancel'))
  })

  // 下一次點擊（pointerdown, pointerup, click）收起預覽
  await act(async () => {
    card.dispatchEvent(ptr('pointerdown'))
    card.dispatchEvent(ptr('pointerup'))
    card.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await settle(50)
  })
  assert.equal(document.querySelector('.shelf-peek'), null, '預覽被收起')
  assert.equal(handed.length, 0, '是收預覽，不是放進對話')
})

test('長按後有 click 的正常順序：那個 click 被吞、預覽留著', async () => {
  useShelf.getState().add([new File([new Uint8Array([137, 80, 78, 71])], 'a.png', { type: 'image/png' })])
  const handed: File[][] = []
  useShelf.getState().setSink({ add: (f) => handed.push(f), label: 'x' })

  await mount(<ImageShelf />)
  const card = document.querySelector<HTMLButtonElement>('.shelf-card-main')!
  assert.ok(card, 'card must exist')

  await act(async () => {
    card.dispatchEvent(ptr('pointerdown'))
    await settle(500)
  })
  assert.ok(document.querySelector('.shelf-peek'), '長按後預覽打開')

  await act(async () => {
    card.dispatchEvent(ptr('pointerup'))
    card.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await settle(50)
  })
  assert.ok(document.querySelector('.shelf-peek'), 'click 被吞，預覽留著')
  assert.equal(handed.length, 0, '沒有放進對話')
})

test('一般點一下照舊放進對話', async () => {
  useShelf.getState().add([new File([new Uint8Array([137, 80, 78, 71])], 'a.png', { type: 'image/png' })])
  const handed: File[][] = []
  useShelf.getState().setSink({ add: (f) => handed.push(f), label: 'x' })

  await mount(<ImageShelf />)
  const card = document.querySelector<HTMLButtonElement>('.shelf-card-main')!
  assert.ok(card, 'card must exist')

  await act(async () => {
    card.dispatchEvent(ptr('pointerdown'))
    card.dispatchEvent(ptr('pointerup'))
    card.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await settle(50)
  })
  assert.equal(handed.length, 1, '一般點擊放進對話')
})
