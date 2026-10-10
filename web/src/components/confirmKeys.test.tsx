/**
 * 確認框的鍵盤與連點審查：輸入法組字中的 Enter、連點確認、預設焦點。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, imeEnter, keydown, mount, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { ConfirmDialog } from './ConfirmDialog'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const dialog = (extra: Partial<Parameters<typeof ConfirmDialog>[0]> = {}) => {
  const calls = { confirm: 0, cancel: 0 }
  const el = (
    <ConfirmDialog open title="刪除專案" body="x" confirmLabel="刪除" danger onConfirm={() => calls.confirm++} onCancel={() => calls.cancel++} {...extra} />
  )
  return { calls, el }
}
const confirmBtn = () => [...document.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent === '刪除')!

test('刪除類確認框預設焦點在「取消」，不在「確定」；Enter 落在取消上等於取消', async () => {
  const { calls, el } = dialog()
  await mount(el)
  assert.equal(document.activeElement?.textContent, '取消')
  await keydown(document.activeElement!, 'Escape')
  assert.equal(calls.cancel, 1)
  assert.equal(calls.confirm, 0)
})

test('要打字確認的框：輸入法組字中的 Enter（選字確認）不能當成確認——名字用中文輸入法打完，選字那一下 Enter 就把專案刪了', async () => {
  const { calls, el } = dialog({ requireText: '我的專案' })
  await mount(el)
  const input = document.querySelector<HTMLInputElement>('.confirm-require input')!
  await typeInto(input, '我的專案')
  await imeEnter(input, 'composing')
  await imeEnter(input, 'keycode229')
  assert.equal(calls.confirm, 0, '選字確認的 Enter 不算')
  await keydown(input, 'Enter')
  assert.equal(calls.confirm, 1, '真正的 Enter 才算')
})

test('連點確認只觸發一次（呼叫端不一定會同步關掉框；兩次刪除請求就是兩次副作用）', async () => {
  const { calls, el } = dialog()
  await mount(el)
  await click(confirmBtn())
  await click(confirmBtn())
  await click(confirmBtn())
  assert.equal(calls.confirm, 1)
})

test('第二個選項（secondary）同樣只觸發一次，而且跟確認共用同一道門：連點兩顆不同的按鈕也只算一次', async () => {
  let secondary = 0
  const { calls, el } = dialog({ secondaryLabel: '全新對話', onSecondary: () => secondary++ })
  await mount(el)
  const sec = [...document.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent === '全新對話')!
  await click(sec)
  await click(sec)
  await click(confirmBtn())
  assert.equal(secondary + calls.confirm, 1)
})

test('要打字確認的框：組字中的 Esc 不取消', async () => {
  const { calls, el } = dialog({ requireText: '我的專案' })
  await mount(el)
  const input = document.querySelector<HTMLInputElement>('.confirm-require input')!
  await typeInto(input, '我的')
  await keydown(input, 'Escape', { isComposing: true })
  assert.equal(calls.cancel, 0)
  assert.equal(input.value, '我的')
  await keydown(input, 'Escape')
  assert.equal(calls.cancel, 1)
})
