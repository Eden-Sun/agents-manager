/**
 * 全域快捷鍵（⌥↑/⌥↓ 換 bot、Ctrl+1…9 跳專案）的誤觸審查：選字中的按鍵、Shift 組合（選取文字）、
 * 疊在上面的對話框（含燈箱，它不是 .modal-backdrop）都不能穿透。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { registerSettingsLeaveGuard } from '../lib/settingsLeaveGuard'
import { dialogOpen } from '../lib/dialogOpen'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const { useBotSwitchKeys } = await import('./useBotSwitchKeys')
const { useProjectJumpKeys } = await import('./useProjectJumpKeys')

function Keys() {
  useBotSwitchKeys()
  useProjectJumpKeys()
  return null
}

function setup() {
  const calls: string[] = []
  useStore.setState({
    selectAdjacentBot: ((dir: number) => calls.push(`bot:${dir}`)) as never,
    selectProject: ((id: string) => calls.push(`project:${id}`)) as never,
    projects: [{ id: 'p1', path: '/p1', label: 'p1', host: 'local' }] as never,
  })
  return calls
}

async function press(init: KeyboardEventInit & { keyCode?: number }, target: EventTarget = document.body) {
  const ev = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init })
  if (init.keyCode !== undefined) Object.defineProperty(ev, 'keyCode', { value: init.keyCode })
  await act(async () => {
    target.dispatchEvent(ev)
  })
  return ev
}

const alt = (key: string, extra: KeyboardEventInit = {}) => ({ key, altKey: true, ...extra })
const ctrl1 = (extra: KeyboardEventInit = {}) => ({ key: '1', code: 'Digit1', ctrlKey: true, ...extra })

test('⌥↓ 換 bot；⌥⇧↓（在輸入框裡是「選取到段落尾」）不能順手換 bot', async () => {
  const calls = setup()
  await mount(<Keys />)
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'])
  await press(alt('ArrowDown', { shiftKey: true }))
  await press(alt('ArrowUp', { shiftKey: true }))
  assert.deepEqual(calls, ['bot:1'], 'Shift 組合是別的意思')
})

test('輸入法選字中（isComposing，或 WebKit 在 compositionend 之後送的 keyCode 229）的方向鍵與數字鍵不觸發快捷鍵', async () => {
  const calls = setup()
  await mount(<Keys />)
  await press(alt('ArrowDown', { isComposing: true }))
  await press(alt('ArrowDown', { keyCode: 229 }))
  await press(ctrl1({ keyCode: 229 }))
  await press(ctrl1({ isComposing: true }))
  assert.deepEqual(calls, [])
})

test('對話框開著時快捷鍵不穿透：確認框、一般 modal、圖片燈箱（.lightbox 不是 .modal-backdrop）、其他 aria-modal', async () => {
  const calls = setup()
  await mount(<Keys />)
  for (const html of [
    '<div class="confirm-backdrop"></div>',
    '<div class="modal-backdrop"></div>',
    '<div class="lightbox" role="dialog" aria-modal="true"></div>',
    '<div role="dialog" aria-modal="true"></div>',
  ]) {
    const el = document.createElement('div')
    el.innerHTML = html
    document.body.appendChild(el)
    await press(alt('ArrowDown'))
    await press(ctrl1())
    el.remove()
  }
  assert.deepEqual(calls, [], '燈箱等疊層開著，背後的 bot／專案不能被換掉')
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'], '關掉之後恢復')
})

test('手機側邊欄抽屜本身（aside.sidebar 的 aria-modal）不算「有對話框」：抽屜開著 Ctrl+1 照樣能跳', async () => {
  const calls = setup()
  await mount(<Keys />)
  const aside = document.createElement('aside')
  aside.className = 'sidebar open'
  aside.setAttribute('aria-modal', 'true')
  document.body.appendChild(aside)
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'])
})

test('側邊欄裡開的對話框（新增 Project／Bot、環境設定的 .modal-backdrop）算「有對話框」：⌥↓／Ctrl+1 不穿透；關掉後恢復（#935）', async () => {
  const calls = setup()
  await mount(<Keys />)
  const aside = document.createElement('aside')
  aside.className = 'sidebar open'
  aside.setAttribute('aria-modal', 'true')
  aside.innerHTML = '<div class="modal-backdrop"><div class="modal" role="dialog" aria-modal="true"></div></div>'
  document.body.appendChild(aside)
  await press(alt('ArrowDown'))
  await press(ctrl1())
  assert.deepEqual(calls, [], '抽屜裡的對話框開著，背後的 bot／專案不能被換掉')
  aside.querySelector('.modal-backdrop')!.remove()
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'], '對話框關掉、只剩抽屜本身：恢復')
  aside.remove()
})

test('抽屜開＋側欄內 modal 開時 Esc 只關 modal、抽屜留著；modal 關了之後 Esc 才關抽屜（#935；抽屜 Esc 條件照 App.tsx）', async () => {
  let drawerClosed = 0
  let modalClosed = 0
  // 跟 App.tsx 的抽屜 Esc 同一個判斷，而且先註冊（抽屜先開、Modal 後掛）。
  const drawerEsc = (e: KeyboardEvent) => {
    if (e.key !== 'Escape' || e.defaultPrevented) return
    if (dialogOpen()) return
    e.preventDefault()
    drawerClosed++
  }
  window.addEventListener('keydown', drawerEsc)
  try {
    const { Modal } = await import('../components/Modal')
    const tree = (open: boolean) => (
      <aside className="sidebar open" aria-modal="true">
        <Modal open={open} title="新增 Bot" onClose={() => modalClosed++}>
          <input />
        </Modal>
      </aside>
    )
    await mount(tree(true))
    const dialog = document.querySelector('.modal')!
    assert.equal(dialogOpen(), true)
    await press({ key: 'Escape' }, dialog)
    assert.deepEqual([modalClosed, drawerClosed], [1, 0], 'Esc 只關最上層的 modal，抽屜留著')
    await unmountAll()
    await mount(tree(false))
    assert.equal(dialogOpen(), false)
    await press({ key: 'Escape' }, document.querySelector('aside.sidebar')!)
    assert.deepEqual([modalClosed, drawerClosed], [1, 1], 'modal 關了之後 Esc 輪到抽屜')
  } finally {
    window.removeEventListener('keydown', drawerEsc)
  }
})

test('桌機設定卡有未儲存變更（守門回 true）：⌥↓ 與 Ctrl+1 都不換 bot／專案，事件仍 preventDefault；解除後恢復（#925）', async () => {
  const calls = setup()
  await mount(<Keys />)
  let asked = 0
  const off = registerSettingsLeaveGuard(() => {
    asked++
    return true
  })
  const down = await press(alt('ArrowDown'))
  const jump = await press(ctrl1())
  assert.deepEqual(calls, [], '守門擋下：沒換 bot、沒換專案')
  assert.equal(down.defaultPrevented, true)
  assert.equal(jump.defaultPrevented, true)
  assert.equal(asked, 2, '兩個快捷鍵都問過守門')
  off()
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'], '解除後恢復')
})

test('守門回 false（沒有未儲存變更）：快捷鍵照常能用', async () => {
  const calls = setup()
  await mount(<Keys />)
  const off = registerSettingsLeaveGuard(() => false)
  await press(alt('ArrowDown'))
  assert.deepEqual(calls, ['bot:1'])
  off()
})

