/**
 * Bot 設定卡（桌機非模態）：開場焦點進表單；關掉（Esc／✕）之後焦點要回到開它的那個鈕，不掉到 body。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mk = (id: string, name: string) =>
  ({ id, name, project_id: 'p1', kind: 'claude', model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false }) as unknown as Bot

/** 開設定的鈕在卡片外面，跟真實畫面一樣（側欄齒輪、標題列）。 */
function Page() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return (
    <>
      <button type="button" id="opener">設定</button>
      {open ? <BotSettingsPanel botId="b1" /> : null}
    </>
  )
}

test('Esc 關掉設定卡：焦點回到開它的鈕，不掉到 body', async () => {
  useStore.setState({
    bots: [mk('b1', 'one')], runs: {}, busy: {}, selectedBotId: 'b1', settingsBotId: null, refreshState: async () => {},
  })
  await mount(<Page />)
  const opener = document.getElementById('opener') as HTMLButtonElement
  opener.focus()
  useStore.setState({ settingsBotId: 'b1' })
  await settle(20)
  assert.ok(document.querySelector('.bot-settings'), '前提：設定卡已開')
  assert.notEqual(document.activeElement, opener, '前提：開場焦點已移進卡片')
  await keydown(document.activeElement!, 'Escape')
  await settle(20)
  assert.equal(document.querySelector('.bot-settings'), null, 'Esc 要關掉')
  assert.equal(document.activeElement, opener, '焦點回到開它的鈕')
})
