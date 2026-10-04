/**
 * 主力晶片長按（2026-10-04 使用者：「手機版主力長按說明顏色意義」）：觸控長按不移動、放開＝開說明（onHold），
 * 長按後移動＝照舊拖曳，不開說明。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { usePinnedDrag } from './usePinnedDrag'
import { ChipLegend } from './ChipLegend'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(teardownDom)

const holds: number[] = []
function Strip({ onHold }: { onHold: () => void }) {
  const dnd = usePinnedDrag(['a', 'b'], ['a', 'b'], { a: 'a', b: 'b' }, () => {}, onHold)
  return (
    <div className="unread-pin-grid">
      {['a', 'b'].map((id) => (
        <button key={id} type="button" className="unread-chip pinned" data-bot-id={id} onPointerDown={(e) => dnd.onPointerDown(e, id)}>
          {id}
        </button>
      ))}
    </div>
  )
}

const ptr = (type: string, x: number, y: number) =>
  new PointerEvent(type, { bubbles: true, clientX: x, clientY: y, pointerType: 'touch', button: 0 })
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms))

test('觸控長按不動、放開：開說明', async () => {
  holds.length = 0
  await mount(<Strip onHold={() => holds.push(1)} />)
  const chip = document.querySelector<HTMLElement>('[data-bot-id="a"]')!
  await act(async () => {
    chip.dispatchEvent(ptr('pointerdown', 10, 10))
    await wait(420)
    window.dispatchEvent(ptr('pointermove', 13, 11))
    window.dispatchEvent(ptr('pointerup', 13, 11))
  })
  assert.equal(holds.length, 1)
})

test('觸控長按後移動：是拖曳，不開說明', async () => {
  holds.length = 0
  await mount(<Strip onHold={() => holds.push(1)} />)
  const chip = document.querySelector<HTMLElement>('[data-bot-id="a"]')!
  await act(async () => {
    chip.dispatchEvent(ptr('pointerdown', 10, 10))
    await wait(420)
    window.dispatchEvent(ptr('pointermove', 60, 10))
    window.dispatchEvent(ptr('pointerup', 60, 10))
  })
  assert.equal(holds.length, 0)
})

test('說明用晶片與燈號的真 class 畫範例，Esc 關掉', async () => {
  let closed = 0
  await mount(<ChipLegend onClose={() => (closed += 1)} />)
  const dialog = document.querySelector('[role="dialog"]')!
  for (const cls of ['needs-reply', 'unread', 'waits-kids', 'working', 'current']) {
    assert.ok(dialog.querySelector(`.unread-chip.${cls}`), `少了 ${cls} 的範例`)
  }
  assert.ok(dialog.querySelector('.lamp-bg'), '背景執行中的燈')
  await act(async () => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
  })
  assert.equal(closed, 1)
})
