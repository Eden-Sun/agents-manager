/**
 * 全域快捷鍵（⌥↑/⌥↓ 換 bot、Ctrl+1…9 跳專案）的誤觸審查：選字中的按鍵、Shift 組合（選取文字）、
 * 疊在上面的對話框（含燈箱，它不是 .modal-backdrop）都不能穿透。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'

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
