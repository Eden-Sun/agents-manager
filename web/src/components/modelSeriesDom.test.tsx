import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiModelFields } from './ModelPicker'
import type { ModelInfo } from '../api/types'
import { t } from '../lib/i18n'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const model = (id: string): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: null,
  efforts: [],
  service_tiers: [],
})

const codexModels: ModelInfo[] = [
  'gpt-6-luna',
  'gpt-6-astra',
  'gpt-6.2-sol',
  'gpt-6.2-astra',
  'gpt-6.1-terra',
  'gpt-6.1-sol',
].map(model)

function setupStore() {
  useStore.setState({ models: { 'codex@local@': codexModels } })
}

function modelRadios(): HTMLElement[] {
  const group = document.querySelector('[role="radiogroup"][aria-label="模型"]')
  assert.ok(group, '應有模型 radiogroup')
  return [...group.querySelectorAll<HTMLElement>('[role="radio"]')]
}

test('新增 Bot（沒選模型）只出每個系列的最新版', async () => {
  setupStore()
  await mount(
    <ApiModelFields kind="codex" host="local" model={null} onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const texts = modelRadios().map((el) => el.textContent?.trim())
  assert.deepEqual(texts, ['6-luna', '6.2-sol', '6.2-astra', '6.1-terra'])
  assert.ok(!texts.some((x) => x?.includes(t('舊版'))), '沒有任何舊版標記')
})

test('設定頁目前用舊版：舊版仍列出、被選中，並標「舊版」', async () => {
  setupStore()
  await mount(
    <ApiModelFields kind="codex" host="local" model="gpt-6-astra" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const radios = modelRadios()
  assert.equal(radios.length, 5, '4 個最新版＋目前設定的舊版')
  const checked = radios.filter((el) => el.getAttribute('aria-checked') === 'true')
  assert.equal(checked.length, 1)
  assert.ok(checked[0].textContent?.includes(t('舊版')), '被選中的舊版要標「舊版」')
  const oldTagged = radios.filter((el) => el.textContent?.includes(t('舊版')))
  assert.equal(oldTagged.length, 1, '只有目前設定的那顆標舊版')
})
