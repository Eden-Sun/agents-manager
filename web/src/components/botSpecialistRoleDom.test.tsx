/**
 * 專家（specialist）角色的設定面板：選了「專家」要先填 track，空白不能儲存；存的是 PATCH {role:"specialist", track}。
 * 錯誤碼（track_required／user_only）要顯示成人話，不是原始代碼。
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

const roleSelect = () => document.querySelector<HTMLSelectElement>('label select')!
const trackInput = () => document.querySelector<HTMLInputElement>('form[aria-label] input[type="text"]')
const saveSpecialist = () => document.querySelector<HTMLButtonElement>('form[aria-label] button[type="submit"]')!

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

test('選專家後沒填 track：儲存停用，且不送出 PATCH', async () => {
  const sent = await setup(mk(null))
  const select = roleSelect()
  await act(async () => {
    select.value = 'specialist'
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })
  assert.ok(trackInput(), '選了專家就出現 track 欄位')
  assert.equal(saveSpecialist().disabled, true, '空白 track 不能儲存')
  await click(saveSpecialist())
  assert.deepEqual(sent, [], '空白不送出')
})

test('填了 track 後儲存：PATCH {role:"specialist", track}', async () => {
  const sent = await setup(mk(null))
  const select = roleSelect()
  await act(async () => {
    select.value = 'specialist'
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await typeInto(trackInput()!, ' infra ')
  await act(async () => {})
  assert.equal(saveSpecialist().disabled, false)
  await click(saveSpecialist())
  assert.deepEqual(sent, [{ role: 'specialist', track: 'infra' }])
})

test('已是專家時 track 欄位帶入現有值', async () => {
  await setup(mk('specialist', 'ui'))
  assert.equal(roleSelect().value, 'specialist')
  assert.equal(trackInput()?.value, 'ui')
})

test('錯誤碼 track_required 與 user_only 顯示成人話', async () => {
  const { ApiError } = await import('../api/types')
  const { errTextForTest } = await import('../store/store')
  assert.match(errTextForTest(new ApiError(400, { error: 'track_required' }, 'bad request')), /專長軌道/)
  assert.match(errTextForTest(new ApiError(403, { error: 'forbidden', reason: 'user_only' }, 'forbidden')), /只有使用者能改/)
})
