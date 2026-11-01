/**
 * 專家模式「新增 Bot」DOM 測試（2026-11-01）：
 * - 一般模式不變
 * - 專家模式角色必選
 * - 專家缺 track 不能送
 * - 執行者選池條目鎖欄位
 * - 已有管理者灰掉
 * - 分享區隱藏
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { Bot, ExecutorPoolEntry, Project } from '../api/types'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string, mode: 'parent_child' | 'expert' = 'parent_child', executor_pool: ExecutorPoolEntry[] = []): Project => ({
  id,
  label: `proj-${id}`,
  path: `/${id}`,
  host: 'local',
  mode,
  executor_pool,
  workspace_id: null,
  github: null,
  created_at: '',
})

const bot = (id: string, projectId: string, role: Bot['role'] = null, name = `bot-${id}`): Bot => ({
  id,
  name,
  project_id: projectId,
  kind: 'claude',
  identity: null,
  model: null,
  effort: null,
  args: [],
  env: {},
  autostart: false,
  inject_hooks: true,
  auto_approve: false,
  managed_by: 'user',
  parent_bot_id: null,
  primary: false,
  primary_position: 0,
  cwd: null,
  role,
  manager_use: true,
  track: null,
  runtime_override: null,
} as unknown as Bot)

let requests: FakeRequest[] = []

beforeEach(() => {
  useStore.setState({
    localTools: {
      claude: { installed: true, path: '/bin/claude', version: '2.1.0', logged_in: true },
      codex: { installed: true, path: '/bin/codex', version: '0.1.0', logged_in: true },
      grok: { installed: true, path: '/bin/grok', version: '1.0.0', logged_in: true },
      agy: { installed: true, path: '/bin/agy', version: '1.0.0', logged_in: true },
    },
  } as never)
  requests = fakeApi((req) => {
    if (req.method === 'POST' && /\/projects\/[^/]+\/bots$/.test(req.path)) {
      return { bot_id: 'new-b1', name: (req.body as { name?: string })?.name || 'created-bot' }
    }
    if (req.method === 'PATCH' && /\/bots\/new-b1$/.test(req.path)) {
      return { needs_restart: false }
    }
    return undefined
  })
})

function setValue(el: HTMLInputElement | HTMLSelectElement, value: string, type: 'input' | 'change' = 'change') {
  const proto = el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype
  const setter = Object.getOwnPropertyDescriptor(proto, 'value')!.set!
  setter.call(el, value)
  el.dispatchEvent(new Event(type, { bubbles: true }))
}

async function change(el: HTMLInputElement | HTMLSelectElement, value: string, type: 'input' | 'change' = 'change') {
  await act(async () => setValue(el, value, type))
  await settle()
}

const openAddBot = async () => {
  await mount(<Sidebar />)
  await settle()
  const open = [...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!
  await click(open)
  await settle()
}

test('一般模式（parent_child）：對話框結構不變，含分享區，不出現專家角色卡片', async () => {
  useStore.setState({
    projects: [project('p-normal', 'parent_child')],
    bots: [bot('b1', 'p-normal')],
    selectedProjectId: 'p-normal',
    selectedBotId: 'b1',
  } as never)

  await openAddBot()

  // 應該沒有專家角色選擇卡
  assert.equal(document.querySelectorAll('.expert-role-picker').length, 0, '一般模式不應有角色選擇')
  // 應該有分享區塊
  const share = [...document.querySelectorAll<HTMLElement>('.field')].find((f) => f.textContent?.includes('用途（分享）') || f.textContent?.includes('分享'))
  assert.ok(share, '一般模式應有分享區塊')
})

test('專家模式：最上面選角色，分享區隱藏', async () => {
  useStore.setState({
    projects: [project('p-expert', 'expert')],
    bots: [],
    selectedProjectId: 'p-expert',
    selectedBotId: null,
  } as never)

  await openAddBot()

  // 出現角色卡片
  const picker = document.querySelector('.expert-role-picker')
  assert.ok(picker, '專家模式應有角色選擇卡')
  assert.equal(picker?.querySelectorAll('.expert-role-card').length, 4, '應有四種角色：管理者、顧問、執行者、專家')

  // 分享區塊隱藏
  const share = [...document.querySelectorAll<HTMLElement>('.field')].find((f) => f.textContent?.includes('用途（分享）') || f.textContent?.includes('建立為分享用 Bot'))
  assert.equal(share, undefined, '專家模式應隱藏分享區塊')
})

test('專家模式：專案已有管理者時管理者灰掉並註明「已有管理者 <名稱>」', async () => {
  useStore.setState({
    projects: [project('p-expert', 'expert')],
    bots: [bot('b-mgr', 'p-expert', 'manager', 'big-boss')],
    selectedProjectId: 'p-expert',
    selectedBotId: 'b-mgr',
  } as never)

  await openAddBot()

  const mgrCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('管理者'))!
  assert.ok(mgrCard.classList.contains('disabled'), '已有管理者時卡片應加上 disabled')
  assert.match(mgrCard.textContent ?? '', /已有管理者 big-boss/)
  const input = mgrCard.querySelector<HTMLInputElement>('input[type="radio"]')!
  assert.equal(input.disabled, true, '已有管理者時 radio 應 disabled')
})

test('專家模式：選專家時出現必填 track 欄位，沒填不能送出；填了之後送出 PATCH 帶 role 與 track', async () => {
  useStore.setState({
    projects: [project('p-expert', 'expert')],
    bots: [],
    selectedProjectId: 'p-expert',
    selectedBotId: null,
  } as never)

  await openAddBot()

  const specCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('專家'))!
  await click(specCard)
  await settle()

  // 應該有 track 欄位
  const trackInput = [...document.querySelectorAll<HTMLInputElement>('input')].find((i) => i.placeholder?.includes('billing'))
  assert.ok(trackInput, '選專家應出現 track 輸入框')

  const submit = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '新增並啟動')!
  assert.equal(submit.disabled, true, '沒填 track 不能送出')

  // 填寫 track
  await change(trackInput, 'billing', 'input')
  await change(trackInput, 'billing', 'change')

  // 名字會預設為 billing
  const nameInput = [...document.querySelectorAll<HTMLInputElement>('input')].find((i) => i.value === 'billing')
  assert.ok(nameInput, '專家預設名稱應為 track')

  assert.equal(submit.disabled, false, '填好 track 且名稱合法後可送出')

  await click(submit)
  await settle(50)

  const post = requests.find((r) => r.method === 'POST' && /\/projects\/p-expert\/bots$/.test(r.path))
  assert.ok(post, '應有 POST 建立 bot')

  const patch = requests.find((r) => r.method === 'PATCH' && /\/bots\/new-b1$/.test(r.path))
  assert.ok(patch, '應有 PATCH /api/bots/{id}')
  const patchBody = patch.body as { role?: string; track?: string; manager_use?: boolean }
  assert.equal(patchBody.role, 'specialist')
  assert.equal(patchBody.track, 'billing')
  assert.equal(patchBody.manager_use, true)
})

test('專家模式：選執行者時列出池條目，選條目鎖定 kind、model、effort', async () => {
  const pool: ExecutorPoolEntry[] = [
    { id: 'codex-pool-1', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 2 },
  ]
  useStore.setState({
    projects: [project('p-expert', 'expert', pool)],
    bots: [],
    selectedProjectId: 'p-expert',
    selectedBotId: null,
  } as never)

  await openAddBot()

  const execCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('執行者'))!
  await click(execCard)
  await settle()

  // 應預選 pool 條目且鎖定 kind / model
  const poolEntryOpt = [...document.querySelectorAll<HTMLButtonElement>('.opt')].find((b) => b.textContent?.includes('codex · gpt-6-luna'))
  assert.ok(poolEntryOpt, '應列出執行者池條目')
  assert.equal(poolEntryOpt.getAttribute('aria-checked'), 'true', '池條目應被預選')

  // kind 按鈕應被 disabled
  const kindBtns = [...document.querySelectorAll<HTMLButtonElement>('[aria-label="kind"] .opt')]
  for (const b of kindBtns) {
    assert.equal(b.disabled, true, '選中池條目時 kind 按鈕應 disabled')
  }

  // 切換到「自訂」
  const customOpt = [...document.querySelectorAll<HTMLButtonElement>('.opt')].find((b) => b.textContent?.trim() === '自訂')!
  await click(customOpt)
  await settle()

  // kind 按鈕不再被 disabled
  const claudeBtn = kindBtns.find((b) => b.textContent?.trim() === 'claude')!
  assert.equal(claudeBtn.disabled, false, '切到自訂後 kind 按鈕可點選')
})

test('專家模式：管理者角色不顯示「可被管理者使用」勾選', async () => {
  useStore.setState({
    projects: [project('p-expert', 'expert')],
    bots: [],
    selectedProjectId: 'p-expert',
    selectedBotId: null,
  } as never)

  await openAddBot()

  const mgrCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('管理者'))!
  await click(mgrCard)
  await settle()

  const mgrUse = [...document.querySelectorAll<HTMLSpanElement>('span')].find((s) => s.textContent?.trim() === '可被管理者使用')
  assert.equal(mgrUse, undefined, '管理者角色不顯示「可被管理者使用」')

  // 切換到顧問
  const advCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('顧問'))!
  await click(advCard)
  await settle()

  const advMgrUse = [...document.querySelectorAll<HTMLSpanElement>('span')].find((s) => s.textContent?.trim() === '可被管理者使用')
  assert.ok(advMgrUse, '顧問角色應顯示「可被管理者使用」')
})

test('專家模式：選專家 → 強度預選 medium；手動改 high 後切換模型仍是 high；改選顧問 → high', async () => {
  useStore.setState({
    projects: [project('p-expert', 'expert')],
    bots: [bot('mgr-1', 'p-expert', 'manager', 'existing-mgr')],
    selectedProjectId: 'p-expert',
    selectedBotId: null,
    models: {
      'claude@local@': [
        {
          id: 'claude-sonnet-4-6',
          display_name: 'Claude Sonnet 4.6',
          description: '',
          is_default: true,
          default_effort: 'medium',
          account_default_effort: 'high',
          efforts: ['low', 'medium', 'high'],
          service_tiers: [],
        },
        {
          id: 'claude-opus-4-6',
          display_name: 'Claude Opus 4.6',
          description: '',
          is_default: false,
          default_effort: 'high',
          account_default_effort: 'high',
          efforts: ['low', 'medium', 'high'],
          service_tiers: [],
        },
      ],
      'codex@local@': [
        {
          id: 'codex-astra',
          display_name: 'Codex Astra',
          description: '',
          is_default: true,
          default_effort: 'high',
          account_default_effort: 'high',
          efforts: ['low', 'medium', 'high'],
          service_tiers: [],
        },
      ],
    },
  } as never)

  await openAddBot()

  // 1. 選專家 → 強度預選 medium（儘管帳號預設是 high，仍應優先選建議值 medium，且標記「建議」）
  const specCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('專家'))!
  await click(specCard)
  await settle()

  const effortGroup = document.querySelector('[role="radiogroup"][aria-label="強度"]')
  assert.ok(effortGroup, '應顯示強度選項組')

  const mediumBtn = [...effortGroup.querySelectorAll<HTMLButtonElement>('.opt')].find((b) => b.textContent?.includes('M'))
  assert.ok(mediumBtn, '應有 medium 按鈕')
  assert.equal(mediumBtn.getAttribute('aria-checked'), 'true', '專家模式下選專家強度應預選 medium')
  assert.ok(mediumBtn.querySelector('.effort-recommended')?.textContent?.includes('建議'), 'medium 應標記「建議」')

  const highBtn = [...effortGroup.querySelectorAll<HTMLButtonElement>('.opt')].find((b) => b.textContent?.includes('H'))
  assert.ok(highBtn, '應有 high 按鈕')
  assert.equal(highBtn.getAttribute('aria-checked'), 'false', 'high 不應被預選')
  assert.ok(highBtn.querySelector('.effort-account')?.textContent?.includes('此帳號預設'), 'high 應保留「此帳號預設」標記')

  // 2. 手動改 high 後切換模型仍是 high
  await click(highBtn)
  await settle()
  assert.equal(highBtn.getAttribute('aria-checked'), 'true', '手動點選後 high 應為 checked')

  // 切換模型到 claude-opus-4-6
  const opusModelBtn = [...document.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="模型"] .opt')].find(
    (b) => b.title?.includes('claude-opus-4-6') || b.textContent?.includes('Opus')
  )
  assert.ok(opusModelBtn, '應有 Opus 模型按鈕')
  await click(opusModelBtn)
  await settle()

  const highBtnAfterModelSwitch = [...document.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="強度"] .opt')].find(
    (b) => b.textContent?.includes('H')
  )
  assert.equal(highBtnAfterModelSwitch?.getAttribute('aria-checked'), 'true', '切換模型後手動選擇的 high 仍應被保留')

  // 3. 改選顧問 → 強度重設並預選顧問的建議值 high
  const advCard = [...document.querySelectorAll<HTMLLabelElement>('.expert-role-card')].find((c) => c.textContent?.includes('顧問'))!
  await click(advCard)
  await settle()

  const advHighBtn = [...document.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="強度"] .opt')].find(
    (b) => b.textContent?.includes('H')
  )
  assert.equal(advHighBtn?.getAttribute('aria-checked'), 'true', '改選顧問後強度應預選 high')
})
