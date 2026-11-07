/**
 * 主力晶片觸控長按（2026-11-07 使用者：「主力長按的功能可以不需要了」，狀態卡拿掉）：
 * 長按 350ms 後才進入拖曳（用來跟捲動分開）；長按不動放開＝什麼都不做，移動＝重排。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { usePinnedDrag } from './usePinnedDrag'
import { ChipLegend } from './ChipLegend'

// 長按計時（~400ms）走手動時鐘：`wait` 只推進虛擬時間。
let timers: ReturnType<typeof installManualTimers>

before(setupDom)
beforeEach(() => {
  timers = installManualTimers()
})
afterEach(async () => {
  timers.restore() // unmountAll 的 settle 要真 timer
  await unmountAll()
})
after(teardownDom)

// 記錄 commit 的版本：拖曳放開時新順序會經由 commit 寫回，用來斷言「別根手指放開」有沒有真的寫回。
const commits: string[][] = []
let consume: (() => boolean) | null = null
function Strip() {
  const dnd = usePinnedDrag(['a', 'b'], ['a', 'b'], { a: 'a', b: 'b' }, (next) => commits.push(next))
  consume = dnd.consumeClick
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

const ptr = (type: string, x: number, y: number, pointerId = 1) =>
  new PointerEvent(type, { bubbles: true, clientX: x, clientY: y, pointerType: 'touch', button: 0, pointerId })
const wait = (ms: number) => timers.clock.advance(ms)

test('觸控長按不動放開：什麼都不做（不重排），且吞掉那下 click', async () => {
  commits.length = 0
  await mount(<Strip />)
  const chip = document.querySelector<HTMLElement>('[data-bot-id="a"]')!
  await act(async () => {
    chip.dispatchEvent(ptr('pointerdown', 10, 10))
    await wait(420)
  })
  assert.equal(consume?.(), true, '長按進入拖曳準備狀態，click 要吞掉')
  assert.equal(document.querySelector('.bot-status-card'), null, '不再開狀態卡')
  await act(async () => {
    window.dispatchEvent(ptr('pointermove', 13, 11))
    window.dispatchEvent(ptr('pointerup', 13, 11))
  })
  assert.deepEqual(commits, [], '沒移動＝不重排')
  await act(async () => {
    await wait(5)
  })
  assert.equal(consume?.(), false, '下一個 task 放行')
})

// happy-dom 的晶片全是 0×0，排序算不出位置；這裡把 a、b 排成一列，讓落點有意義。
function placeChips() {
  const at: Record<string, number> = { a: 0, b: 60 }
  for (const id of Object.keys(at)) {
    const el = document.querySelector<HTMLElement>(`[data-bot-id="${id}"]`)!
    const left = at[id]
    el.getBoundingClientRect = () => ({ x: left, y: 0, left, right: left + 50, top: 0, bottom: 30, width: 50, height: 30, toJSON: () => ({}) }) as DOMRect
  }
}

test('拖曳中別根手指放開不算：不 commit，也不結束這次拖曳', async () => {
  commits.length = 0
  await mount(<Strip />)
  placeChips()
  const chip = document.querySelector<HTMLElement>('[data-bot-id="a"]')!
  await act(async () => {
    chip.dispatchEvent(ptr('pointerdown', 10, 10, 1))
    await wait(420)
    window.dispatchEvent(ptr('pointermove', 200, 10, 1))
    // 第二根手指碰一下放開：不能用它的座標 commit，也不能把拖曳收掉。
    window.dispatchEvent(ptr('pointerup', 10, 10, 2))
  })
  assert.equal(commits.length, 0, '別根手指放開不 commit')
  await act(async () => {
    window.dispatchEvent(ptr('pointerup', 200, 10, 1))
  })
  assert.deepEqual(commits, [['b', 'a']], '按下的那根手指放開才 commit，新順序把 a 放到最後')
})

test('拖曳中別根手指移動不改落點', async () => {
  commits.length = 0
  await mount(<Strip />)
  placeChips()
  const chip = document.querySelector<HTMLElement>('[data-bot-id="a"]')!
  await act(async () => {
    chip.dispatchEvent(ptr('pointerdown', 10, 10, 1))
    await wait(420)
    window.dispatchEvent(ptr('pointermove', 86, 10, 1))
    // 第二根手指拖到最左邊：不能把落點拉回 a 前面。86 是落在「b 前面」與「最後面」之間黏滯範圍內的位置，拿別根手指的座標就會黏到前面。
    window.dispatchEvent(ptr('pointermove', 5, 10, 2))
    window.dispatchEvent(ptr('pointerup', 86, 10, 1))
  })
  assert.deepEqual(commits, [['b', 'a']], '落點只跟著按下的那根手指')
})

test('說明用晶片與燈號的真 class 畫範例，Esc 關掉', async () => {
  let closed = 0
  await mount(<ChipLegend onClose={() => (closed += 1)} />)
  const dialog = document.querySelector('[role="dialog"]')!
  for (const cls of ['needs-reply', 'unread', 'waits-kids', 'working', 'current']) {
    assert.ok(dialog.querySelector(`.unread-chip.${cls}`), `少了 ${cls} 的範例`)
  }
  assert.ok(dialog.querySelector('.lamp-bg'), '子 agent 還在跑的轉圈燈')
  await act(async () => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
  })
  assert.equal(closed, 1)
})
