import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useTapCopy } from './useTapCopy'

const copies: string[] = []
let realExecCommand: typeof document.execCommand

before(() => {
  setupDom()
  realExecCommand = document.execCommand
  document.execCommand = ((command: string) => {
    copies.push(command)
    return true
  }) as typeof document.execCommand
})
afterEach(async () => {
  copies.length = 0
  await unmountAll()
})
after(async () => {
  document.execCommand = realExecCommand
  await teardownDom()
})

function Probe({ onCopied }: { onCopied: (ok: boolean) => void }) {
  const tap = useTapCopy('訊息全文', onCopied)
  return (
    <div className="bubble" {...tap}>
      訊息全文
      <div className="lightbox"><div className="lightbox-inner" /></div>
    </div>
  )
}

const pointer = (type: string) =>
  new PointerEvent(type, { bubbles: true, isPrimary: true, button: 0, pointerId: 1 })
const click = () => new MouseEvent('click', { bubbles: true })
const tick = () => new Promise((resolve) => setTimeout(resolve, 0))

test('點燈箱背景不複製訊息', async () => {
  const copied: boolean[] = []
  await mount(<Probe onCopied={(ok) => copied.push(ok)} />)
  const background = document.querySelector<HTMLElement>('.lightbox')!
  await act(async () => {
    background.dispatchEvent(pointer('pointerdown'))
    background.dispatchEvent(pointer('pointerup'))
    background.dispatchEvent(click())
    await tick()
  })
  assert.deepEqual(copies, [])
  assert.deepEqual(copied, [])
})

test('點泡泡文字照樣複製', async () => {
  const copied: boolean[] = []
  await mount(<Probe onCopied={(ok) => copied.push(ok)} />)
  const bubble = document.querySelector<HTMLElement>('.bubble')!
  await act(async () => {
    bubble.dispatchEvent(pointer('pointerdown'))
    bubble.dispatchEvent(pointer('pointerup'))
    bubble.dispatchEvent(click())
    await tick()
  })
  assert.deepEqual(copies, ['copy'])
  assert.deepEqual(copied, [true])
})
