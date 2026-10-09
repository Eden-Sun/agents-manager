/**
 * `useEnterCommit` 接在真的 input 上：改名是點了才畫出 input（掛載時 ref 還是 null），
 * 監聽器必須在那之後仍然掛上（Android 軟鍵盤 Enter 走 beforeinput）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useRef, useState } from 'react'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useEnterCommit } from './useEnterCommit'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

/** 先畫按鈕，點了才畫 input（同 BotNameField 的編輯流程）。 */
function Probe({ onCommit }: { onCommit: () => void }) {
  const [editing, setEditing] = useState(false)
  const ref = useRef<HTMLInputElement>(null)
  const enter = useEnterCommit(ref, onCommit)
  if (!editing) return <button type="button" onClick={() => setEditing(true)}>name</button>
  return <input ref={ref} type="text" {...enter} />
}

test('input 在掛載之後才出現（改名進入編輯）：beforeinput 的換行仍會 commit', async () => {
  let commits = 0
  const root = await mount(<Probe onCommit={() => commits++} />)
  await click(root.querySelector('button')!)
  const input = root.querySelector('input')!
  assert.ok(input, '點了之後應該畫出 input')

  const ev = new InputEvent('beforeinput', { inputType: 'insertLineBreak', bubbles: true, cancelable: true })
  input.dispatchEvent(ev)
  assert.equal(commits, 1)
  assert.equal(ev.defaultPrevented, true)
})

test('input 在掛載之後才出現：一般文字輸入不 commit、也不擋', async () => {
  let commits = 0
  const root = await mount(<Probe onCommit={() => commits++} />)
  await click(root.querySelector('button')!)
  const input = root.querySelector('input')!

  const ev = new InputEvent('beforeinput', { inputType: 'insertText', data: 'a', bubbles: true, cancelable: true })
  input.dispatchEvent(ev)
  assert.equal(commits, 0)
  assert.equal(ev.defaultPrevented, false)
})
