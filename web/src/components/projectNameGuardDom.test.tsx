/** 專案改名打到一半（輸入框開著、字跟原本不同）：關分頁會丟掉，要被 `beforeunload` 攔下；沒改或收起來就不攔。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { ProjectNameField } from './ProjectNameField'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const unload = () => {
  const ev = new Event('beforeunload', { cancelable: true })
  window.dispatchEvent(ev)
  return ev.defaultPrevented
}

test('改名輸入框的字跟原本不同才攔；改回去就不攔', async () => {
  await mount(<ProjectNameField projectId="p1" label="原名" editing onEditing={() => {}} />)
  const input = document.querySelector<HTMLInputElement>('input')!
  assert.equal(unload(), false, '還沒改')
  await typeInto(input, '新名字')
  assert.equal(unload(), true, '改了沒送出')
  await typeInto(input, '原名')
  assert.equal(unload(), false, '改回原樣')
})

test('沒在編輯（輸入框沒開）就不攔', async () => {
  await mount(<ProjectNameField projectId="p1" label="原名" editing={false} onEditing={() => {}} />)
  assert.equal(unload(), false)
})
