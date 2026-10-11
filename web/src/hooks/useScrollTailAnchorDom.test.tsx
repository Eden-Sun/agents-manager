/**
 * #1239：「載入更早的訊息」前面插進一頁，人要留在原來看的那一則，不能被推到剛載入那頁的最舊一則。
 * happy-dom 不排版，這裡自己量：容器可視高 300、每則 100px、第 i 則的 top＝i*100 − 容器的 scrollTop。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
import { act, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { useScrollTail } from './useScrollTail'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const ROW = 100
const VIEW = 300

/** 跟 ChatPanel／GroupChatPanel 一樣：容器掛 ref＋onScroll，每則帶 data-msg-id。 */
function List({ initial, onReady }: { initial: string[]; onReady: (set: (ids: string[]) => void) => void }) {
  const [ids, set] = useState(initial)
  useEffect(() => onReady(set), [onReady, set])
  const tail = useScrollTail([ids])
  return (
    <div data-list="" ref={tail.ref} onScroll={tail.onScroll} style={{ height: VIEW, overflow: 'auto' }}>
      {ids.map((id) => (
        <div key={id} data-msg-id={id} style={{ height: ROW }}>
          {id}
        </div>
      ))}
    </div>
  )
}

const rect = (top: number, bottom: number) => ({ top, bottom, left: 0, right: 0, width: 0, height: bottom - top, x: 0, y: 0, toJSON() {} }) as DOMRect

/**
 * 找到 box 的 getBoundingClientRect 實際生效的那一層。別的測試檔會在 HTMLElement.prototype 上放 own property
 * （還原時也可能留下），只改 Element.prototype 會被它擋住，量出來全是 0（#1239 疊在 #1246 上紅過）。
 */
function rectOwner(box: HTMLElement): object {
  let o: object | null = box
  while (o && !Object.prototype.hasOwnProperty.call(o, 'getBoundingClientRect')) o = Object.getPrototypeOf(o)
  return o ?? Element.prototype
}

/**
 * happy-dom 不排版：容器的 scrollHeight／clientHeight 與每則的 getBoundingClientRect 照 scrollTop 算。
 * 回傳還原函式：把那一層原本的 descriptor 原樣放回（沒有 own property 就刪掉）。
 */
function stubLayout(box: HTMLElement): () => void {
  const owner = rectOwner(box)
  const prior = Object.getOwnPropertyDescriptor(owner, 'getBoundingClientRect')
  const realRect = (prior?.value ?? Element.prototype.getBoundingClientRect) as () => DOMRect
  const rows = () => Array.from(box.children) as HTMLElement[]
  Object.defineProperty(box, 'scrollHeight', { configurable: true, get: () => rows().length * ROW })
  Object.defineProperty(box, 'clientHeight', { configurable: true, get: () => VIEW })
  Object.defineProperty(owner, 'getBoundingClientRect', {
    configurable: true,
    writable: true,
    value: function (this: Element) {
      if (this === box) return rect(0, VIEW)
      if (this.parentElement !== box || !this.hasAttribute('data-msg-id')) return realRect.call(this)
      const top = rows().indexOf(this as HTMLElement) * ROW - box.scrollTop
      return rect(top, top + ROW)
    },
  })
  return () => {
    if (prior) Object.defineProperty(owner, 'getBoundingClientRect', prior)
    else delete (owner as Record<string, unknown>).getBoundingClientRect
  }
}

async function scrollTo(box: HTMLElement, top: number) {
  box.scrollTop = top
  await act(async () => {
    box.dispatchEvent(new Event('scroll', { bubbles: true }))
  })
}

const ids = (prefix: string, n: number) => Array.from({ length: n }, (_, i) => `${prefix}${i}`)

/** 掛上清單、量尺寸、回傳容器與「改清單」的函式。 */
async function setup(initial: string[]) {
  let setIds: (ids: string[]) => void = () => {}
  const host = await mount(<List initial={initial} onReady={(s) => (setIds = s)} />)
  const box = host.querySelector<HTMLElement>('[data-list]')!
  const restore = stubLayout(box)
  const setList = async (next: string[]) => {
    await act(async () => setIds(next))
    await settle()
  }
  return { box, setList, restore }
}

test('前面插入舊訊息時，人留在原來那一則（scrollTop 補上多出來的高度）', async () => {
  const { box, setList, restore } = await setup(ids('m', 5))
  try {
    await scrollTo(box, 0)
    await setList([...ids('o', 3), ...ids('m', 5)])
    assert.equal(box.scrollTop, 300, '原本在 m0 最上面，前面多 3 則（300px），scrollTop 要跟著補 300')
  } finally {
    restore()
  }
})

test('不貼底時尾端多一則不動', async () => {
  const { box, setList, restore } = await setup(ids('m', 5))
  try {
    await scrollTo(box, 0)
    await setList(ids('m', 6))
    assert.equal(box.scrollTop, 0)
  } finally {
    restore()
  }
})

test('貼底時尾端多一則仍貼底（既有行為）', async () => {
  const { box, setList, restore } = await setup(ids('m', 5))
  try {
    await scrollTo(box, 200)
    await setList(ids('m', 6))
    assert.equal(box.scrollTop, box.scrollHeight)
  } finally {
    restore()
  }
})
