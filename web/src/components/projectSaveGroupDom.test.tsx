/**
 * 專案設定的一顆儲存：執行者池、副管理者、角色設定各自不再有儲存鈕；
 * 底部儲存把有變更的區塊合併成一次 PATCH，「還原」清掉所有草稿。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll, click } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, PatchProjectInput, Project } from '../api/types'
import { ProjectExpertSettings } from './ProjectExpertSettings'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const project = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', executor_pool: [], role_policy: {}, deputy: null } as unknown as Project
const bot = (id: string, role: Bot['role']) => ({ id, name: id, role, project_id: 'p1', kind: 'claude' }) as unknown as Bot

async function setup() {
  const sent: PatchProjectInput[] = []
  useStore.setState({
    projects: [project],
    bots: [bot('mgr', 'manager'), bot('adv', 'advisor')],
    loadModels: async () => {},
    patchProject: async (_id: string, input: PatchProjectInput) => {
      sent.push(input)
      return true
    },
  } as never)
  await mount(<ProjectExpertSettings projectId="p1" />)
  return sent
}

const buttons = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('button')].filter((b) => b.textContent?.trim() === label)
const deputySelect = () => document.querySelector<HTMLSelectElement>('.project-deputy-editor select')!

test('只有一顆儲存；改副管理者＋角色設定後一次 PATCH 兩塊', async () => {
  const sent = await setup()
  assert.equal(buttons('儲存執行者池').length, 0)
  assert.equal(buttons('儲存角色設定').length, 0)
  assert.equal(buttons('儲存').length, 1)
  assert.equal(buttons('儲存')[0].disabled, true, '沒變更不能按')

  await act(async () => {
    deputySelect().value = 'adv'
    deputySelect().dispatchEvent(new Event('change', { bubbles: true }))
  })
  await click(buttons('全部加入建議')[0])
  await act(async () => {})
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /副管理者/)
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /角色設定/)

  await click(buttons('儲存')[0])
  await act(async () => {})
  assert.equal(sent.length, 1, '合併成一次 PATCH')
  assert.deepEqual(sent[0].deputy, { bot_id: 'adv', after_sec: 180 })
  assert.ok(sent[0].role_policy?.manager, '角色設定一起送')
  assert.equal('executor_pool' in sent[0], false, '沒改的區塊不送')
})

test('還原清掉所有草稿', async () => {
  const sent = await setup()
  await click(buttons('全部加入建議')[0])
  await act(async () => {})
  assert.equal(buttons('儲存')[0].disabled, false)
  await click(buttons('還原')[0])
  await act(async () => {})
  assert.equal(buttons('儲存')[0].disabled, true)
  assert.deepEqual(sent, [])
})
