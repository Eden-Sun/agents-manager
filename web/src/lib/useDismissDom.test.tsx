import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useRef, useState } from 'react'
import { useDismiss } from './useDismiss'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

function Probe({
  initialOpen = true,
  onClose,
}: {
  initialOpen?: boolean
  onClose: () => void
}) {
  const [open] = useState(initialOpen)
  const ref = useRef<HTMLDivElement>(null)
  useDismiss(open, ref, onClose)

  return (
    <div>
      <div id="outside">Outside</div>
      <div id="inside" ref={ref}>
        <button id="inside-btn" type="button">Inside</button>
      </div>
    </div>
  )
}

test('點外面關：open=true 時點擊 ref 外面的元素觸發 close', async () => {
  let closed = 0
  await mount(<Probe initialOpen={true} onClose={() => { closed++ }} />)
  const outside = document.querySelector<HTMLElement>('#outside')!
  outside.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
  assert.equal(closed, 1)
})

test('點裡面不關：點擊 ref 元素本身或其子元素不觸發 close', async () => {
  let closed = 0
  await mount(<Probe initialOpen={true} onClose={() => { closed++ }} />)
  const insideBtn = document.querySelector<HTMLElement>('#inside-btn')!
  insideBtn.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
  const inside = document.querySelector<HTMLElement>('#inside')!
  inside.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
  assert.equal(closed, 0)
})

test('Esc 關：按下 Escape 鍵觸發 close，其他鍵不觸發', async () => {
  let closed = 0
  await mount(<Probe initialOpen={true} onClose={() => { closed++ }} />)
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }))
  assert.equal(closed, 0)
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  assert.equal(closed, 1)
})

test('關閉時不掛監聽：open=false 時 mousedown 與 Escape 均不觸發 close，且未在 document 註冊監聽', async () => {
  let closed = 0
  const addSpy: string[] = []
  const origAdd = document.addEventListener.bind(document)
  document.addEventListener = ((type: string, listener: EventListenerOrEventListenerObject, options?: boolean | AddEventListenerOptions) => {
    addSpy.push(type)
    return origAdd(type as keyof DocumentEventMap, listener as EventListener, options)
  }) as typeof document.addEventListener

  try {
    await mount(<Probe initialOpen={false} onClose={() => { closed++ }} />)
    assert.equal(addSpy.includes('mousedown'), false)
    assert.equal(addSpy.includes('keydown'), false)

    document.querySelector<HTMLElement>('#outside')!.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    assert.equal(closed, 0)
  } finally {
    document.addEventListener = origAdd
  }
})

test('卸載移除監聽：元件卸載後移除 mousedown 與 keydown 監聽，後續事件不再觸發 close', async () => {
  let closed = 0
  const removeSpy: string[] = []
  const origRemove = document.removeEventListener.bind(document)
  document.removeEventListener = ((type: string, listener: EventListenerOrEventListenerObject, options?: boolean | EventListenerOptions) => {
    removeSpy.push(type)
    return origRemove(type as keyof DocumentEventMap, listener as EventListener, options)
  }) as typeof document.removeEventListener

  try {
    await mount(<Probe initialOpen={true} onClose={() => { closed++ }} />)
    await unmountAll()
    assert.ok(removeSpy.includes('mousedown'), '卸載時應移除 mousedown 監聽')
    assert.ok(removeSpy.includes('keydown'), '卸載時應移除 keydown 監聽')

    document.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    assert.equal(closed, 0)
  } finally {
    document.removeEventListener = origRemove
  }
})
