/**
 * Bot 設定表單的名稱欄：跟 daemon `valid_bot_name` 同一套規則（1–32 字、不含 `@ , : ;`、不前後空白、空白不連續）。
 * 規則不符時「儲存」要停用、畫面要講原因；不能讓使用者按了才被 daemon 回 400。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, typeInto, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'

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

function Harness() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

const nameInput = () => document.querySelector<HTMLInputElement>('#bot-settings-form-b1 input[type="text"]')!
const saveButton = () => document.querySelector<HTMLButtonElement>('button[form="bot-settings-form-b1"]')!

async function setup() {
  useStore.setState({
    bots: [mk('b1', 'one')],
    runs: {},
    busy: {},
    selectedBotId: 'b1',
    settingsBotId: 'b1',
    refreshState: async () => {},
  })
  await mount(<Harness />)
}

test('名稱含 daemon 不收的字元（@）：儲存停用，不送出去才被 400', async () => {
  await setup()
  await typeInto(nameInput(), 'ops@1')
  await act(async () => {})
  assert.equal(saveButton().disabled, true)
})

test('名稱超過 32 字：儲存停用', async () => {
  await setup()
  await typeInto(nameInput(), 'x'.repeat(33))
  await act(async () => {})
  assert.equal(saveButton().disabled, true)
})

test('名稱前後有空白：儲存停用（daemon 會回 400）', async () => {
  await setup()
  await typeInto(nameInput(), ' ops')
  await act(async () => {})
  assert.equal(saveButton().disabled, true)
})

test('合法的新名稱：儲存可按', async () => {
  await setup()
  await typeInto(nameInput(), 'ops bot')
  await act(async () => {})
  assert.equal(saveButton().disabled, false)
})
