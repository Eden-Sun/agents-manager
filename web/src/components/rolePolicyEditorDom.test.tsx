/**
 * 角色設定（B 期）：「加入建議」只帶入草稿、不送出；儲存才 PATCH 整份 role_policy（清掉的欄位不送）；超出範圍停用儲存。
 * Bot 設定的「執行期覆寫」：改動跟著底部的儲存一起送 runtime_override（清掉＝null）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, typeInto, setupDom, teardownDom, unmountAll, click } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, PatchBotInput, PatchProjectInput, Project } from '../api/types'
import { RolePolicyEditor } from './RolePolicyEditor'
import { BotSettingsPanel } from './BotSettingsPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const expert = (role_policy: Project['role_policy'] = {}) =>
  ({ id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', executor_pool: [], role_policy }) as unknown as Project

const card = (label: string) => document.querySelector<HTMLElement>(`.role-policy-editor section[aria-label="${label}"]`)!
const pill = (root: ParentNode, label: string) => [...root.querySelectorAll<HTMLButtonElement>('button[role="radio"]')].find((b) => b.textContent?.trim() === label)!
const button = (root: ParentNode, label: string) => [...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === label)!
const saveBtn = () => button(document, '儲存角色設定')

async function setupEditor(project: Project) {
  const sent: PatchProjectInput[] = []
  useStore.setState({
    projects: [project],
    patchProject: async (_id: string, input: PatchProjectInput) => {
      sent.push(input)
      return true
    },
  } as never)
  await mount(<RolePolicyEditor project={project} />)
  return sent
}

test('全部加入建議只帶入草稿，不送出；儲存才送整份 role_policy（沒設的欄位不送）', async () => {
  const sent = await setupEditor(expert())
  assert.equal(document.querySelectorAll('.role-policy-editor button').length > 0, true)
  assert.equal([...document.querySelectorAll('.rp-card button')].some((b) => b.textContent?.includes('建議')), false, '卡片上不再各放一顆建議')
  await click(button(document, '全部加入建議'))
  await act(async () => {})
  assert.equal(pill(card('管理者'), '開').getAttribute('aria-checked'), 'true', '管理者建議：保溫開')
  assert.deepEqual(sent, [], '建議不自動存')
  assert.equal(saveBtn().disabled, false, '有變更，儲存可按')
  await click(saveBtn())
  assert.deepEqual(sent, [{ role_policy: { manager: { keep_warm: true }, specialist: { context_tokens: 500000, keep_warm: true }, advisor: { keep_warm: false }, executor: { context_tokens: 200000, keep_warm: false } } }])
})

test('保溫關：不顯示保溫分鐘', async () => {
  await setupEditor(expert({ advisor: { keep_warm: false } }))
  assert.doesNotMatch(card('顧問').textContent ?? '', /保溫分鐘/)
  assert.match(card('管理者').textContent ?? '', /保溫分鐘/)
})

test('超出範圍：儲存停用並提示欄位', async () => {
  await setupEditor(expert())
  const minutes = card('顧問').querySelector<HTMLInputElement>('input[type="number"]')!
  await typeInto(minutes, '60')
  await act(async () => {})
  assert.equal(saveBtn().disabled, true)
  assert.match(card('顧問').textContent ?? '', /超出範圍/)
})

test('已存的政策：卡片顯示存的值，「還原」回到存的值', async () => {
  await setupEditor(expert({ specialist: { context_tokens: 512000, keep_warm: true } }))
  assert.equal(card('專家').querySelector('.rt-ctx-value')?.textContent, '512k')
  assert.equal(pill(card('專家'), '開').getAttribute('aria-checked'), 'true')
  await click(pill(card('專家'), '關'))
  await act(async () => {})
  assert.equal(saveBtn().disabled, false)
  await click(button(document, '還原'))
  await act(async () => {})
  assert.equal(pill(card('專家'), '開').getAttribute('aria-checked'), 'true', '還原回存的值')
})

test('保溫只影響 claude 的說明只在「角色設定」說明講一次，卡片上不重複', async () => {
  await setupEditor(expert())
  assert.match(document.querySelector('.role-policy-editor .hint')?.textContent ?? '', /codex 永快取不需保溫/)
  assert.doesNotMatch(card('執行者').textContent ?? '', /codex/)
  assert.doesNotMatch(card('管理者').textContent ?? '', /codex/)
})

function Harness() {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId="b1" /> : null
}

const bot = (runtime_override: Bot['runtime_override']) =>
  ({ id: 'b1', name: 'one', project_id: 'p1', kind: 'claude', role: 'manager', model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false, runtime_override }) as unknown as Bot

test('Bot 執行期覆寫：清掉＝送 null；改動跟底部的儲存一起送', async () => {
  const sent: PatchBotInput[] = []
  useStore.setState({
    bots: [bot({ keep_warm: false })],
    projects: [expert()],
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
  assert.equal(document.querySelector('.rt-override')?.textContent?.includes('codex'), false, 'claude bot 不顯示 codex 提示')
  const section = document.querySelector<HTMLElement>('.rt-override')!
  assert.equal(pill(section, '關').getAttribute('aria-checked'), 'true', '覆寫存的值')
  await click(pill(section, '跟角色'))
  await act(async () => {})
  const save = document.querySelector<HTMLButtonElement>('button[form="bot-settings-form-b1"]')!
  assert.equal(save.disabled, false)
  await click(save)
  assert.deepEqual(sent, [{ runtime_override: null }], '全部清掉＝null（跟角色）')
})

test('角色卡快取命中率：取該專案同角色 Bot 的 run.prompt_cache.hit_ratio 平均；沒資料畫「—」', async () => {
  const mk = (id: string, role: Bot['role'], project_id = 'p1') => ({ id, name: id, project_id, kind: 'claude', role }) as unknown as Bot
  const pc = (hit_ratio: number | null) => ({ id: 'r', state: 'running', prompt_cache: { hit_ratio } })
  useStore.setState({
    bots: [mk('m1', 'manager'), mk('e1', 'executor'), mk('e2', 'executor'), mk('e3', 'executor', 'other'), mk('a1', 'advisor')],
    runs: { m1: pc(0.994), e1: pc(0.9), e2: pc(0.5), e3: pc(0), a1: pc(null) },
  } as never)
  await mount(<RolePolicyEditor project={expert()} />)
  const cache = (label: string) => card(label).querySelector('.rp-cache')!.textContent!.replace(/\s+/g, ' ').trim()
  assert.match(cache('管理者'), /99%$/)
  assert.match(cache('執行者'), /70%$/, '只算同專案：(0.9+0.5)/2')
  assert.match(cache('顧問'), /—$/, 'hit_ratio 為 null')
  assert.match(cache('專家'), /—$/, '沒有這個角色的 Bot')
})
