/**
 * Bot 名稱欄的錯誤提示（#1524 加的那句）要跟欄位關聯：輸入不合規時欄位 aria-invalid，提示字用 aria-describedby 指到，
 * 欄位名稱仍只是「名稱」（提示字不能混進 label 的名字裡唸兩次）。Bot 設定表單與新增 Bot 表單都要對。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { BOT_NAME_HINT } from '../lib/botName'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'
import { Sidebar } from './Sidebar'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

/** 欄位本身：`label.field` 裡第一個 span 是「名稱」的那個。 */
function nameField(): HTMLInputElement {
  const label = [...document.querySelectorAll<HTMLLabelElement>('label.field')].find((l) => l.querySelector('span')?.textContent?.trim() === '名稱')!
  return label.querySelector<HTMLInputElement>('input')!
}

function assertInvalidNameAssociated(input: HTMLInputElement) {
  assert.equal(input.getAttribute('aria-invalid'), 'true', '不合規要標 aria-invalid')
  const hintId = input.getAttribute('aria-describedby')
  const hint = hintId ? document.getElementById(hintId) : null
  assert.equal(hint?.textContent?.trim(), BOT_NAME_HINT, 'aria-describedby 要指到那句提示')
  const labelId = input.getAttribute('aria-labelledby')
  const name = labelId ? document.getElementById(labelId)?.textContent : input.closest('label')?.textContent
  assert.equal(name?.trim(), '名稱', '欄位名稱只能是「名稱」，提示字不能混進名字裡')
}

const mk = (id: string, name: string) =>
  ({ id, name, project_id: 'p1', kind: 'claude', model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false }) as unknown as Bot

function SettingsHarness() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

test('Bot 設定：名稱不合規時欄位標 aria-invalid，提示字關聯到欄位；合規時都不標', async () => {
  useStore.setState({
    bots: [mk('b1', 'one')], runs: {}, busy: {}, selectedBotId: 'b1', settingsBotId: 'b1', refreshState: async () => {},
  })
  await mount(<SettingsHarness />)
  await typeInto(nameField(), 'ops@1')
  await act(async () => {})
  assertInvalidNameAssociated(nameField())

  await typeInto(nameField(), 'ops bot')
  await act(async () => {})
  assert.equal(nameField().hasAttribute('aria-invalid'), false, '合規就不標')
  assert.equal(nameField().getAttribute('aria-describedby'), null, '合規就不掛提示')
})

test('新增 Bot：名稱不合規時欄位標 aria-invalid，提示字關聯到欄位', async () => {
  fakeApi(() => undefined)
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local' }],
    bots: [], identities: [], runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
  await mount(<Sidebar />)
  await settle()
  const open = [...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!
  await click(open)
  await settle()
  await typeInto(nameField(), 'ops@1')
  await act(async () => {})
  assertInvalidNameAssociated(nameField())
})
