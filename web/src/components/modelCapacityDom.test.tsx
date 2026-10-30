/**
 * #1674：codex「模型滿載」要有獨立的標示——側欄（字＋紫燈）、晶片（紫框＋說明）、對話面板輸入框上方一行；
 * 跟「⚠ 中斷」「要你回答」「撞額度」分開，下一個回合開始（daemon 清掉 turn_error／turn_error_kind）就消失。
 * 種類看 daemon 給的 `turn_error_kind`，不比對文字。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { toRun } from '../api/normalize'
import { Sidebar } from './Sidebar'
import { UnreadChip } from './UnreadChip'
import { ModelCapacityNote } from './ModelCapacityNote'
import { TurnErrorBadge } from './TurnErrorBadge'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const project = { id: 'p1', label: 'proj', path: '/p1', host: 'local' }
const bot = (id: string, extra: Record<string, unknown> = {}) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'codex', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null,
  primary: false, primary_position: 0, cwd: null, ...extra,
})
const CAPACITY_TEXT = '模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）'
const run = (botId: string, extra: Record<string, unknown> = {}) =>
  ({ id: `run-${botId}`, bot_id: botId, state: 'running', agent_status: 'idle', workspace_id: null, pane_id: null, started_at: new Date().toISOString(), ...extra })
const capacityRun = (botId: string) => run(botId, { turn_error: CAPACITY_TEXT, turn_error_kind: 'model_at_capacity' })
const apiErrorRun = (botId: string) => run(botId, { turn_error: 'API Error: Connection lost mid-response.', turn_error_kind: 'api_error' })

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project],
    bots: [bot('cap', { primary: true }), bot('err', { primary: true }), bot('ok', { primary: true })],
    runs: { cap: capacityRun('cap'), err: apiErrorRun('err'), ok: run('ok') },
    hosts: [], botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'ok', selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

const rowOf = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"]`)!
const chipOf = (name: string) => document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${name.replace(/^bot-/, '')}"]`)

test('normalize：turn_error_kind 照 daemon 給的收；舊 daemon 沒帶是 null', () => {
  const base = { id: 'r', bot_id: 'b', state: 'running', agent_status: 'idle', started_at: '2026-10-13T00:00:00Z' }
  assert.equal(toRun({ ...base, turn_error: CAPACITY_TEXT, turn_error_kind: 'model_at_capacity' })?.turn_error_kind, 'model_at_capacity')
  assert.equal(toRun({ ...base })?.turn_error_kind, null)
})

test('側欄：滿載的列出現「模型滿載」＋紫燈，不出「⚠ 中斷」；一般 API 錯誤仍是「⚠ 中斷」', async () => {
  await mount(<Sidebar />)
  await settle()
  const cap = rowOf('cap')
  assert.equal(cap.querySelector('.bot-capacity')?.textContent, '模型滿載')
  assert.equal(cap.querySelector('.bot-turn-error'), null, '滿載不重複畫「⚠ 中斷」')
  assert.ok(cap.querySelector('.lamp.lamp-capacity'), '閒置燈換紫')
  const err = rowOf('err')
  assert.equal(err.querySelector('.bot-capacity'), null)
  assert.equal(err.querySelector('.bot-turn-error')?.textContent?.trim(), '⚠ 中斷')
  assert.equal(err.querySelector('.lamp-capacity'), null)
  assert.equal(rowOf('ok').querySelector('.bot-capacity'), null)
})

test('晶片：滿載是 at-capacity 專屬 class（不是 needs-reply／unread／working）；一般 API 錯誤沒有', async () => {
  await mount(<UnreadChip />)
  await settle(50)
  const cap = chipOf('bot-cap')!
  assert.ok(cap.classList.contains('at-capacity'), cap.className)
  assert.ok(!cap.classList.contains('needs-reply') && !cap.classList.contains('working'), cap.className)
  assert.ok(cap.title.includes('模型滿載'), cap.title)
  assert.ok(cap.querySelector('.sr-only')?.textContent?.includes('模型滿載'))
  assert.ok(!chipOf('bot-err')!.classList.contains('at-capacity'))
})

test('晶片：要你回答（blocked）比模型滿載急，不被它蓋掉', async () => {
  useStore.setState({ runs: { cap: { ...capacityRun('cap'), agent_status: 'blocked' }, err: apiErrorRun('err'), ok: run('ok') } } as never)
  await mount(<UnreadChip />)
  await settle(50)
  const cap = chipOf('bot-cap')!
  assert.ok(cap.classList.contains('needs-reply') && !cap.classList.contains('at-capacity'), cap.className)
})

test('對話面板：輸入框上方一行說明（含換模型提示）；標題列的「回合被中斷」badge 讓位', async () => {
  await mount(
    <>
      <TurnErrorBadge botId="cap" />
      <ModelCapacityNote botId="cap" />
    </>,
  )
  const note = document.querySelector<HTMLElement>('.composer-capacity')!
  assert.ok(note, '要有說明行')
  assert.ok(note.textContent?.includes('模型滿載') && note.textContent.includes('換一個模型') && note.textContent.includes('稍後再送'), note.textContent ?? '')
  assert.equal(document.querySelector('.turn-error-badge'), null)
})

test('一般 API 錯誤與沒有錯誤：沒有說明行；TurnErrorBadge 照舊', async () => {
  await mount(
    <>
      <ModelCapacityNote botId="err" />
      <ModelCapacityNote botId="ok" />
      <TurnErrorBadge botId="err" />
    </>,
  )
  assert.equal(document.querySelector('.composer-capacity'), null)
  assert.ok(document.querySelector('.turn-error-badge'))
})

test('下一個回合開始（turn_error 與種類被清掉）：側欄、晶片、說明行都消失', async () => {
  await mount(
    <>
      <Sidebar />
      <UnreadChip />
      <ModelCapacityNote botId="cap" />
    </>,
  )
  await settle(50)
  assert.ok(rowOf('cap').querySelector('.bot-capacity') && chipOf('bot-cap')!.classList.contains('at-capacity') && document.querySelector('.composer-capacity'))
  await act(() => {
    useStore.setState({ runs: { cap: run('cap', { agent_status: 'working', turn_error: null, turn_error_kind: null }), err: apiErrorRun('err'), ok: run('ok') } } as never)
  })
  await settle(50)
  assert.equal(rowOf('cap').querySelector('.bot-capacity'), null)
  assert.equal(rowOf('cap').querySelector('.lamp-capacity'), null)
  assert.ok(!chipOf('bot-cap')!.classList.contains('at-capacity'))
  assert.equal(document.querySelector('.composer-capacity'), null)
})
