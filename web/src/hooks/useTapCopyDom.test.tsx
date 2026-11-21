import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useTapCopy } from './useTapCopy'

const copies: string[] = []
let realExecCommand: typeof document.execCommand

before(async () => {
  await setupDom()
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
  const tap = useTapCopy('hello', onCopied)
  return (
    <div className="msg" {...tap}>
      回覆內容
    </div>
  )
}

const ptr = (type: string, pointerType: string, x = 10, y = 10) =>
  new PointerEvent(type, { bubbles: true, clientX: x, clientY: y, pointerType, isPrimary: true, button: 0, pointerId: 1 })
const leave = (pointerType: string) => ptr('pointerout', pointerType)
const click = () => new MouseEvent('click', { bubbles: true })
const tick = () => new Promise((r) => setTimeout(r, 0))

test('觸控點一下：pointerdown → pointerup → pointerleave → click 仍會複製', async () => {
  const copied: boolean[] = []
  await mount(<Probe onCopied={(ok) => copied.push(ok)} />)
  const el = document.querySelector<HTMLElement>('.msg')!
  await act(async () => {
    el.dispatchEvent(ptr('pointerdown', 'touch'))
    el.dispatchEvent(ptr('pointerup', 'touch'))
    el.dispatchEvent(leave('touch'))
    el.dispatchEvent(click())
    await tick()
  })
  assert.deepEqual(copies, ['copy'])
  assert.deepEqual(copied, [true])
})

test('觸控拖走（移動超過 8px）：不複製，leave 也不影響這個判斷', async () => {
  const copied: boolean[] = []
  await mount(<Probe onCopied={(ok) => copied.push(ok)} />)
  const el = document.querySelector<HTMLElement>('.msg')!
  await act(async () => {
    el.dispatchEvent(ptr('pointerdown', 'touch', 10, 10))
    el.dispatchEvent(ptr('pointermove', 'touch', 40, 10))
    el.dispatchEvent(ptr('pointerup', 'touch', 40, 10))
    el.dispatchEvent(leave('touch'))
    el.dispatchEvent(click())
    await tick()
  })
  assert.deepEqual(copies, [])
  assert.deepEqual(copied, [])
})

test('滑鼠按下後移出元件再放開：不複製', async () => {
  const copied: boolean[] = []
  await mount(<Probe onCopied={(ok) => copied.push(ok)} />)
  const el = document.querySelector<HTMLElement>('.msg')!
  await act(async () => {
    el.dispatchEvent(ptr('pointerdown', 'mouse'))
    el.dispatchEvent(leave('mouse'))
    el.dispatchEvent(click())
    await tick()
  })
  assert.deepEqual(copies, [])
  assert.deepEqual(copied, [])
})
