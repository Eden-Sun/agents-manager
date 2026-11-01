/**
 * #925：桌機 Bot 設定卡是非模態的，`dialogOpen()` 看不到它。有未儲存變更時，鍵盤導覽（⌥↑／⌥↓）
 * 要先問「放棄未儲存的變更？」，跟滑鼠點外面一致；沒有變更或放棄之後才換 bot。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, click, keydown, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'
import { useBotSwitchKeys } from '../hooks/useBotSwitchKeys'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mk = (id: string, name: string) =>
  ({ id, name, project_id: 'p1', kind: 'claude', model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false }) as unknown as Bot

/** 跟 App 一樣：設定卡只在 `settingsBotId` 指到它時掛著，關掉就卸載（守門跟著解除）。 */
function Harness() {
  useBotSwitchKeys()
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

const text = () => document.body.textContent ?? ''
const buttonByText = (label: string) =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim().startsWith(label))!

async function altDown() {
  const ev = new KeyboardEvent('keydown', { key: 'ArrowDown', altKey: true, bubbles: true, cancelable: true })
  await act(async () => {
    window.dispatchEvent(ev)
  })
  return ev
}

test('有未儲存變更時 ⌥↓ 先彈「放棄未儲存的變更？」、不換 bot；放棄並關閉之後再按才換', async () => {
  globalThis.fetch = (async () => new Response('{}', { status: 200 })) as unknown as typeof fetch
  const switched: number[] = []
  useStore.setState({
    bots: [mk('b1', 'one'), mk('b2', 'two')],
    runs: {},
    busy: {},
    selectedBotId: 'b1',
    settingsBotId: 'b1',
    refreshState: async () => {},
    selectAdjacentBot: ((dir: number) => switched.push(dir)) as never,
  })
  await mount(<Harness />)

  // 沒有變更：不擋。
  const free = await altDown()
  assert.deepEqual(switched, [1], '沒有未儲存變更：⌥↓ 照常換 bot')
  assert.equal(free.defaultPrevented, true)
  switched.length = 0
  assert.ok(!text().includes('放棄未儲存的變更？'))

  await click(buttonByText('High'))
  const blocked = await altDown()
  await until(() => text().includes('放棄未儲存的變更？'), '確認框出現')
  assert.deepEqual(switched, [], '有未儲存變更：沒有換 bot')
  assert.equal(useStore.getState().selectedBotId, 'b1')
  assert.equal(blocked.defaultPrevented, true)

  await click(buttonByText('放棄並關閉'))
  await until(() => !text().includes('放棄未儲存的變更？'), '確認框關了')
  assert.equal(useStore.getState().settingsBotId, null, '設定卡關了')
  await altDown()
  assert.deepEqual(switched, [1], '放棄之後卡片卸載、守門解除：再按 ⌥↓ 才換 bot')
})

test('設定卡：組字中的 Esc 不關面板', async () => {
  globalThis.fetch = (async () => new Response('{}', { status: 200 })) as unknown as typeof fetch
  useStore.setState({
    bots: [mk('b1', 'one')],
    runs: {},
    busy: {},
    selectedBotId: 'b1',
    settingsBotId: 'b1',
    refreshState: async () => {},
  })
  await mount(<Harness />)
  const nameInput = document.querySelector<HTMLInputElement>('input[type="text"]')!
  assert.ok(nameInput)
  await keydown(nameInput, 'Escape', { isComposing: true })
  assert.equal(useStore.getState().settingsBotId, 'b1')
  await keydown(nameInput, 'Escape')
  assert.equal(useStore.getState().settingsBotId, null)
})
