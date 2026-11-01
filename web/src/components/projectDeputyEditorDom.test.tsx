/**
 * 副管理者（專案設定）：下拉只列顧問／專家；門檻最小 60；儲存才 PATCH {deputy}（選「不設」＝null）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, typeInto, setupDom, teardownDom, unmountAll, click } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, PatchProjectInput, Project } from '../api/types'
import { ProjectDeputyEditor } from './ProjectDeputyEditor'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const project = (deputy: Project['deputy'] = null) =>
  ({ id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', deputy }) as unknown as Project
const bot = (id: string, role: Bot['role']) => ({ id, name: id, role, project_id: 'p1' }) as unknown as Bot
const BOTS = [bot('mgr', 'manager'), bot('adv', 'advisor'), bot('spc', 'specialist'), bot('exe', 'executor')]

async function setup(p: Project) {
  const sent: PatchProjectInput[] = []
  useStore.setState({
    projects: [p],
    patchProject: async (_id: string, input: PatchProjectInput) => {
      sent.push(input)
      return true
    },
  } as never)
  await mount(<ProjectDeputyEditor project={p} bots={BOTS} />)
  return sent
}

const select = () => document.querySelector<HTMLSelectElement>('select')!
const number = () => document.querySelector<HTMLInputElement>('input[type="number"]')!
const saveBtn = () => [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!

async function pick(value: string) {
  await act(async () => {
    select().value = value
    select().dispatchEvent(new Event('change', { bubbles: true }))
  })
}

test('下拉只列顧問與專家，管理者與執行者不能當副管理者', async () => {
  await setup(project())
  const values = [...select().options].map((o) => o.value)
  assert.deepEqual(values, ['', 'adv', 'spc'])
})

test('選人、儲存才送 PATCH，預設門檻 180 秒', async () => {
  const sent = await setup(project())
  assert.equal(saveBtn().disabled, true, '沒變更不能存')
  await pick('adv')
  assert.deepEqual(sent, [], '只改草稿不送出')
  assert.equal(number().value, '180')
  await click(saveBtn())
  assert.deepEqual(sent, [{ deputy: { bot_id: 'adv', after_sec: 180 } }])
})

test('門檻小於 60：停用儲存並提示', async () => {
  const sent = await setup(project())
  await pick('spc')
  await typeInto(number(), '59')
  await act(async () => {})
  assert.equal(saveBtn().disabled, true)
  assert.match(document.body.textContent ?? '', /門檻最小 60 秒/)
  await typeInto(number(), '60')
  await act(async () => {})
  assert.equal(saveBtn().disabled, false)
  await click(saveBtn())
  assert.deepEqual(sent, [{ deputy: { bot_id: 'spc', after_sec: 60 } }])
})

test('已設定：顯示存的值；選「不設」送 null', async () => {
  const sent = await setup(project({ bot_id: 'adv', after_sec: 90 }))
  assert.equal(select().value, 'adv')
  assert.equal(number().value, '90')
  await pick('')
  assert.equal(number().disabled, true, '沒選人時門檻停用')
  await click(saveBtn())
  assert.deepEqual(sent, [{ deputy: null }])
})
