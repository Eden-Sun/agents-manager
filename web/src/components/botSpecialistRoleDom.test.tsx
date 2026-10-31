/**
 * 專家（specialist）在設定框：角色是表單的一欄（pill 按鈕），選「專家」才出現「負責任務（track）」，
 * 空白時底部的「儲存」停用；存的是一次 PATCH {role, track}。錯誤碼 track_required／user_only 顯示成人話。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, typeInto, setupDom, teardownDom, unmountAll, click } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, PatchBotInput, Project } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mk = (role: Bot['role'], track: string | null = null) =>
  ({ id: 'b1', name: 'one', project_id: 'p1', kind: 'claude', role, track, model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false }) as unknown as Bot

const expert = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '' } as unknown as Project

function Harness() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

const pill = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('.role-field button[role="radio"]')].find((b) => b.textContent?.trim() === label)!
const trackInput = () => document.querySelector<HTMLInputElement>('.track-field input')
const saveButton = () => document.querySelector<HTMLButtonElement>('button[form="bot-settings-form-b1"]')!

async function setup(bot: Bot) {
  const sent: PatchBotInput[] = []
  useStore.setState({
    bots: [bot],
    projects: [expert],
    runs: {},
    busy: {},
    selectedBotId: 'b1',
    settingsBotId: 'b1',
    refreshState: async () => {},
    patchBot: async (_id: string, input: PatchBotInput) => {
      sent.push(input)
      return { needsRestart: false, deferred: false, remappedModel: null }
    },
  } as never)
  await mount(<Harness />)
  return sent
}

test('選專家後出現 track 欄；空白時儲存停用，且不送出', async () => {
  const sent = await setup(mk(null))
  await click(pill('專家'))
  assert.ok(trackInput(), '選了專家就出現 track 欄位')
  assert.equal(saveButton().disabled, true, '空白 track 不能儲存')
  assert.match(document.querySelector('.track-field')?.textContent ?? '', /空白不能儲存/)
  assert.deepEqual(sent, [], '選角色本身不送出，要按儲存')
})

test('填了 track 後按底部儲存：一次 PATCH {role:"specialist", track}', async () => {
  const sent = await setup(mk(null))
  await click(pill('專家'))
  await typeInto(trackInput()!, ' infra ')
  await act(async () => {})
  assert.equal(saveButton().disabled, false)
  await click(saveButton())
  assert.deepEqual(sent, [{ role: 'specialist', track: 'infra' }])
})

test('已是專家時 track 欄位帶入現有值，選其他角色不出現 track 欄', async () => {
  await setup(mk('specialist', 'ui'))
  assert.equal(pill('專家').getAttribute('aria-checked'), 'true')
  assert.equal(trackInput()?.value, 'ui')
  await click(pill('顧問'))
  assert.equal(trackInput(), null)
})

test('錯誤碼 track_required 與 user_only 顯示成人話', async () => {
  const { ApiError } = await import('../api/types')
  const { errTextForTest } = await import('../store/store')
  assert.match(errTextForTest(new ApiError(400, { error: 'track_required' }, 'bad request')), /負責任務/)
  assert.match(errTextForTest(new ApiError(403, { error: 'forbidden', reason: 'user_only' }, 'forbidden')), /只有使用者能改/)
})
