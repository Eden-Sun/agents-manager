/**
 * #1218：桌機 Bot 設定卡是非模態的，點外面就關。通知（`.notices`）是疊在上面的浮層，不是「外面的畫面」：
 * 按通知的 ✕／動作鈕不能把設定卡關掉，也不能跳「放棄未儲存的變更？」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, typeInto, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'
import { Notices } from '../App'

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

/** 跟 App 一樣：設定卡只在 `settingsBotId` 指到它時掛著。 */
function Harness() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

const text = () => document.body.textContent ?? ''
const pointerDown = (el: Element) =>
  act(() => {
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, pointerType: 'mouse', button: 0, pointerId: 1 }))
  })

function setup() {
  globalThis.fetch = (async () => new Response('{}', { status: 200 })) as unknown as typeof fetch
  useStore.setState({
    bots: [mk('b1', 'alpha')],
    runs: {},
    busy: {},
    selectedBotId: 'b1',
    settingsBotId: 'b1',
    notices: [],
    refreshState: async () => {},
  })
}

test('按通知的 ✕：設定卡留著', async () => {
  setup()
  await mount(
    <>
      <Harness />
      <Notices />
    </>,
  )
  act(() => useStore.getState().notify('info', 'hello'))
  await until(() => document.querySelector('.notice') !== null, '通知出現')
  await pointerDown(document.querySelector('.notice button[aria-label="關閉"]')!)
  assert.equal(useStore.getState().settingsBotId, 'b1', '設定卡沒被關掉')
  assert.ok(document.querySelector('.bot-settings'), '設定卡還在畫面上')
})

test('有未儲存變更時按通知：不跳「放棄未儲存的變更？」', async () => {
  setup()
  await mount(
    <>
      <Harness />
      <Notices />
    </>,
  )
  await typeInto(document.querySelector<HTMLInputElement>('input[type="text"]')!, 'alpha-改')
  act(() => useStore.getState().notify('info', 'hello'))
  await until(() => document.querySelector('.notice') !== null, '通知出現')
  await pointerDown(document.querySelector('.notice button[aria-label="關閉"]')!)
  assert.ok(!text().includes('放棄未儲存的變更？'), '按通知不該問要不要放棄變更')
  assert.equal(useStore.getState().settingsBotId, 'b1')
})

test('點真正的外面照舊會關', async () => {
  setup()
  await mount(<Harness />)
  await pointerDown(document.body)
  assert.equal(useStore.getState().settingsBotId, null)
})
