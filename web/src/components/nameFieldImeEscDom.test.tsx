/**
 * 就地改名欄位（BotNameField／ProjectNameField）在輸入法選字中的 Esc 行為審查。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { click, keydown, mount, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { BotNameField } from './BotNameField'
import { ProjectNameField } from './ProjectNameField'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

test('BotNameField：組字中的 Esc 不關輸入框', async () => {
  let patched = 0
  useStore.setState({
    patchBot: (async () => {
      patched++
      return null
    }) as never,
  })
  await mount(<BotNameField botId="b1" name="舊名" />)
  await click(document.querySelector<HTMLButtonElement>('.bot-name-btn')!)
  const input = document.querySelector<HTMLInputElement>('input[aria-label="Bot 名稱"]')!
  assert.ok(input, '點擊後應出現輸入框')
  await typeInto(input, '新名')
  await keydown(input, 'Escape', { isComposing: true })
  const stillInput = document.querySelector<HTMLInputElement>('input[aria-label="Bot 名稱"]')
  assert.ok(stillInput, '組字中 Esc 輸入框應留著')
  assert.equal(stillInput.value, '新名')
  await keydown(stillInput, 'Escape')
  assert.equal(document.querySelector('input[aria-label="Bot 名稱"]'), null, '一般 Esc 輸入框關閉')
  const btn = document.querySelector<HTMLButtonElement>('.bot-name-btn')
  assert.ok(btn?.textContent?.includes('舊名'), '按鈕文字應含舊名')
  assert.equal(patched, 0, 'Esc 放棄不應呼叫 patchBot')
})

function ProjectWrapper() {
  const [editing, setEditing] = useState(false)
  return <ProjectNameField projectId="p1" label="舊專案" editing={editing} onEditing={setEditing} />
}

test('ProjectNameField：組字中的 Esc 不關輸入框', async () => {
  let patched = 0
  useStore.setState({
    patchProject: (async () => {
      patched++
      return false
    }) as never,
  })
  await mount(<ProjectWrapper />)
  await click(document.querySelector<HTMLButtonElement>('.project-label-head-btn')!)
  const input = document.querySelector<HTMLInputElement>('input[aria-label="Project 名稱"]')!
  assert.ok(input, '點擊後應出現輸入框')
  await typeInto(input, '新專案')
  await keydown(input, 'Escape', { isComposing: true })
  const stillInput = document.querySelector<HTMLInputElement>('input[aria-label="Project 名稱"]')
  assert.ok(stillInput, '組字中 Esc 輸入框應留著')
  assert.equal(stillInput.value, '新專案')
  await keydown(stillInput, 'Escape')
  assert.equal(document.querySelector('input[aria-label="Project 名稱"]'), null, '一般 Esc 輸入框關閉')
  const btn = document.querySelector<HTMLButtonElement>('.project-label-head-btn')
  assert.ok(btn?.textContent?.includes('舊專案'), '按鈕文字應含舊專案')
  assert.equal(patched, 0, 'Esc 放棄不應呼叫 patchProject')
})
