/**
 * 巢狀 Modal：內層子元件即使在 mount 時主動 focus 自己內部的輸入框，
 * 關閉內層後焦點仍須回到外層開啟按鈕，不掉到 body；外層隨後按 Escape 仍可正常關閉。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState, useRef, useEffect } from 'react'
import { keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { Modal } from './Modal'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

function InnerForm({ onDone }: { onDone: () => void }) {
  const inputRef = useRef<HTMLInputElement>(null)
  useEffect(() => {
    inputRef.current?.focus()
  }, [])
  return (
    <form onSubmit={(e) => { e.preventDefault(); onDone() }}>
      <input ref={inputRef} id="inner-input" placeholder="名稱" />
      <button type="submit" id="inner-submit">確定</button>
    </form>
  )
}

function NestedModalPage() {
  const [outerOpen, setOuterOpen] = useState(false)
  const [innerOpen, setInnerOpen] = useState(false)

  return (
    <div>
      <button type="button" id="open-outer" onClick={() => setOuterOpen(true)}>
        開外層
      </button>

      <Modal open={outerOpen} title="外層視窗" onClose={() => setOuterOpen(false)}>
        <button type="button" id="open-inner" onClick={() => setInnerOpen(true)}>
          建立專用 Bot…
        </button>
      </Modal>

      <Modal open={innerOpen} title="新增專用 Bot" onClose={() => setInnerOpen(false)}>
        <InnerForm onDone={() => setInnerOpen(false)} />
      </Modal>
    </div>
  )
}

test('巢狀 Modal 內層子元件 mount autofocus：關閉內層後焦點回到外層按鈕，下一次 Escape 可關外層 (#1848)', async () => {
  await mount(<NestedModalPage />)

  // 1. 打開外層
  const openOuterBtn = document.getElementById('open-outer') as HTMLButtonElement
  openOuterBtn.focus()
  openOuterBtn.click()
  await settle(20)

  const openInnerBtn = document.getElementById('open-inner') as HTMLButtonElement
  assert.ok(openInnerBtn, '外層已開')

  // 2. 點擊「建立專用 Bot…」打開內層
  openInnerBtn.focus()
  openInnerBtn.click()
  await settle(20)

  const innerInput = document.getElementById('inner-input') as HTMLInputElement
  assert.ok(innerInput, '內層已開')
  assert.equal(document.activeElement, innerInput, '內層 input 獲得焦點')

  // 3. 按 Escape 關閉內層
  await keydown(document.activeElement!, 'Escape')
  await settle(20)

  assert.equal(document.getElementById('inner-input'), null, '內層已關閉')
  assert.equal(document.activeElement, openInnerBtn, '關閉內層後焦點回到外層按鈕，不掉到 body')

  // 4. 再按 Escape 關閉外層
  await keydown(document.activeElement!, 'Escape')
  await settle(20)

  assert.equal(document.getElementById('open-inner'), null, '外層已關閉')
  assert.equal(document.activeElement, openOuterBtn, '外層關閉後焦點回到開外層按鈕')
})
