/**
 * openai-api bot kind（切片 E）DOM 測試：
 * - 新增表單：OpenAI 沒啟用或沒金鑰時整顆停用並提示；啟用＋有金鑰後可選
 * - 側欄 KindTag 標籤為 OpenAI（`.kind-tag.openai-api`）
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { Project } from '../api/types'
import { Sidebar } from './Sidebar'
import { KindTag } from './KindTag'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string): Project => ({
  id,
  label: `proj-${id}`,
  path: `/${id}`,
  host: 'local',
  mode: 'expert',
  workspace_id: null,
  github: null,
  created_at: '',
})

let requests: FakeRequest[] = []
let openaiSettings: Record<string, unknown> = {}

beforeEach(() => {
  useStore.setState({
    localTools: {
      claude: { installed: true, path: '/bin/claude', version: '2.1.0', logged_in: true },
      codex: { installed: true, path: '/bin/codex', version: '0.1.0', logged_in: true },
      grok: { installed: true, path: '/bin/grok', version: '1.0.0', logged_in: true },
      agy: { installed: true, path: '/bin/agy', version: '1.0.0', logged_in: true },
    },
    projects: [project('p-oa')],
    bots: [],
    selectedProjectId: 'p-oa',
    selectedBotId: null,
  } as never)
  requests = fakeApi((req) => {
    if (req.method === 'GET' && /\/openai-api\/settings$/.test(req.path)) return openaiSettings
    return undefined
  })
})

const openAddBot = async () => {
  await mount(<Sidebar />)
  await settle()
  const open = [...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!
  await click(open)
  await settle()
  // openai-api 只能當顧問：先選顧問角色，kind 按鈕才會因設定而停用／啟用。
  const advisor = [...document.querySelectorAll<HTMLElement>('.expert-role-card')].find((c) => c.textContent?.includes('顧問'))
  assert.ok(advisor, '專家模式應有顧問角色卡')
  await click(advisor)
  await settle()
}

const openaiOption = () =>
  [...document.querySelectorAll<HTMLButtonElement>('.opt-group.kinds button.opt')].find((b) => b.querySelector('.opt-label')?.textContent === 'openai-api')

test('新增表單：OpenAI 沒啟用時 openai-api 停用並提示去設定', async () => {
  openaiSettings = { enabled: false, key_present: true, key_file: '', key_error: null, total_cap_usd: null, total_cap_since: null, spent_usd: 0, spent_bots_usd: 0, spent_uses_usd: 0 }
  await openAddBot()
  const btn = openaiOption()
  assert.ok(btn, '應有 openai-api 選項')
  assert.equal(btn.disabled, true, '沒啟用時不可選')
  assert.equal(btn.title, '先到 環境設定 → OpenAI API 啟用並設定金鑰', '停用的按鈕應說明去設定')
  assert.ok(requests.some((r) => /\/openai-api\/settings$/.test(r.path)), '應讀 OpenAI 設定')
})

test('新增表單：沒金鑰時 openai-api 也停用', async () => {
  openaiSettings = { enabled: true, key_present: false, key_file: '', key_error: 'missing', total_cap_usd: null, total_cap_since: null, spent_usd: 0, spent_bots_usd: 0, spent_uses_usd: 0 }
  await openAddBot()
  assert.equal(openaiOption()?.disabled, true, '沒金鑰時不可選')
})

test('新增表單：啟用且有金鑰後 openai-api 可選，不再出現提示', async () => {
  openaiSettings = { enabled: true, key_present: true, key_file: '', key_error: null, total_cap_usd: null, total_cap_since: null, spent_usd: 0, spent_bots_usd: 0, spent_uses_usd: 0 }
  await openAddBot()
  assert.equal(openaiOption()?.disabled, false, '就緒後可選')
  assert.ok(!openaiOption()?.title.includes('先到 環境設定'), '就緒後不該提示')
})

test('KindTag：openai-api 顯示 OpenAI 標籤並帶 .kind-tag.openai-api', async () => {
  await mount(<KindTag kind="openai-api" />)
  await settle()
  const tag = document.querySelector('.kind-tag.openai-api')
  assert.ok(tag, '應有 .kind-tag.openai-api')
  assert.equal(tag?.getAttribute('aria-label'), 'OpenAI')
})

test('KindTag（圖示模式）：openai-api 是 GPT 商標＋右下角小「API」字，不分顏色的 codex 同色', async () => {
  useStore.setState({ kindDisplay: 'icon' } as never)
  await mount(<KindTag kind="openai-api" />)
  await settle()
  const tag = document.querySelector('.kind-tag.openai-api')
  assert.ok(tag, '應有 .kind-tag.openai-api')
  const text = tag!.querySelector('svg text')
  assert.equal(text?.textContent, 'API', '右下角有 API 字樣')
  assert.ok(tag!.querySelector('svg path'), '仍是 GPT 商標路徑')
  useStore.setState({ kindDisplay: 'text' } as never)
})
