/**
 * `useStickToBottom`：終端快照的 pre 每張新快照都跟著捲到底；使用者往上捲過就不再搶捲軸，捲回底部附近就恢復
 * （2026-11-05 使用者截圖：需要回應面板停在畫面頂端的工具輸出，提示在看不到的下面）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useRef, useState } from 'react'
import { act, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStickToBottom } from './useStickToBottom'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const ctl: { set?: (v: string) => void } = {}
function Probe({ enabled = true }: { enabled?: boolean }) {
  const ref = useRef<HTMLPreElement>(null)
  const [text, setText] = useState('first')
  ctl.set = setText
  const { onScroll } = useStickToBottom(ref, text, enabled)
  return <pre ref={ref} onScroll={onScroll}>{text}</pre>
}

/** happy-dom 不排版：scrollHeight／clientHeight 用固定值，scrollTop 可寫。 */
function fakeLayout(pre: HTMLElement) {
  Object.defineProperty(pre, 'scrollHeight', { configurable: true, get: () => 1000 })
  Object.defineProperty(pre, 'clientHeight', { configurable: true, get: () => 300 })
  let top = 0
  Object.defineProperty(pre, 'scrollTop', { configurable: true, get: () => top, set: (v: number) => { top = v } })
}

async function scrollTo(pre: HTMLElement, top: number) {
  pre.scrollTop = top
  await act(async () => { pre.dispatchEvent(new window.Event('scroll', { bubbles: true })) })
}

test('新快照到了：沒往上捲就捲到底', async () => {
  await mount(<Probe />)
  const pre = document.querySelector('pre')!
  fakeLayout(pre)
  await act(async () => ctl.set!('second'))
  await settle(3)
  assert.equal(pre.scrollTop, 1000, '要捲到 scrollHeight')
})

test('往上捲過：新快照不再搶捲軸；捲回底部附近就恢復跟隨', async () => {
  await mount(<Probe />)
  const pre = document.querySelector('pre')!
  fakeLayout(pre)
  await scrollTo(pre, 100)
  await act(async () => ctl.set!('second'))
  await settle(3)
  assert.equal(pre.scrollTop, 100, '使用者往上捲了，不能被拉回底部')
  await scrollTo(pre, 690) // 1000 - 690 - 300 = 10px：算在底部
  await act(async () => ctl.set!('third'))
  await settle(3)
  assert.equal(pre.scrollTop, 1000)
})

test('enabled=false：什麼都不動', async () => {
  await mount(<Probe enabled={false} />)
  const pre = document.querySelector('pre')!
  fakeLayout(pre)
  await act(async () => ctl.set!('second'))
  await settle(3)
  assert.equal(pre.scrollTop, 0)
})
