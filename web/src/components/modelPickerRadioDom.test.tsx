import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiModelFields } from './ModelPicker'
import { effortLabel, type ModelInfo } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const testModels: ModelInfo[] = [
  {
    id: 'alpha',
    display_name: 'Alpha',
    description: 'Alpha model',
    is_default: true,
    default_effort: 'medium',
    efforts: ['low', 'medium', 'high'],
    service_tiers: [],
  },
  {
    id: 'beta',
    display_name: 'Beta',
    description: 'Beta model',
    is_default: false,
    default_effort: 'medium',
    efforts: ['low', 'medium', 'high'],
    service_tiers: [],
  },
]

function setupStore() {
  useStore.setState({
    models: {
      'codex@local@': testModels,
    },
  })
}

test('模型選項是 radio，只有選中的那顆 aria-checked=true', async () => {
  setupStore()
  await mount(
    <ApiModelFields
      kind="codex"
      host="local"
      model="beta"
      onModel={() => {}}
      effort="high"
      onEffort={() => {}}
      fast={false}
      onFast={() => {}}
    />,
  )

  const group = document.querySelector('[role="radiogroup"][aria-label="模型"]')
  assert.ok(group, '應有 aria-label="模型" 的 radiogroup')

  const radios = group.querySelectorAll('[role="radio"]')
  assert.equal(radios.length, testModels.length, 'radio 數量應等於模型數')

  const checked = group.querySelectorAll('[role="radio"][aria-checked="true"]')
  assert.equal(checked.length, 1, '應恰好有一顆 radio 是 aria-checked="true"')
  assert.equal(checked[0].textContent?.trim(), 'Beta')
})

test('強度選項同理：[aria-label="強度"] [role="radio"][aria-checked="true"] 恰好一顆', async () => {
  setupStore()
  await mount(
    <ApiModelFields
      kind="codex"
      host="local"
      model="o3-mini"
      onModel={() => {}}
      effort="high"
      onEffort={() => {}}
      fast={false}
      onFast={() => {}}
    />,
  )

  const group = document.querySelector('[role="radiogroup"][aria-label="強度"]')
  assert.ok(group, '應有 aria-label="強度" 的 radiogroup')

  const radios = group.querySelectorAll('[role="radio"]')
  assert.equal(radios.length, 3, 'radio 數量應等於強度選項數')

  const checked = group.querySelectorAll('[role="radio"][aria-checked="true"]')
  assert.equal(checked.length, 1, '應恰好有一顆 radio 是 aria-checked="true"')
  assert.ok(
    checked[0].textContent?.includes(effortLabel('high')),
    `文字應包含 high 對應標籤 ${effortLabel('high')}`,
  )
})

test('沒指定模型時標在預設那顆', async () => {
  setupStore()
  await mount(
    <ApiModelFields
      kind="codex"
      host="local"
      model={null}
      onModel={() => {}}
      effort={null}
      onEffort={() => {}}
      fast={false}
      onFast={() => {}}
    />,
  )

  const group = document.querySelector('[role="radiogroup"][aria-label="模型"]')
  assert.ok(group, '應有 aria-label="模型" 的 radiogroup')

  const checked = group.querySelectorAll('[role="radio"][aria-checked="true"]')
  assert.equal(checked.length, 1, '應恰好有一顆 radio 是 aria-checked="true"')
  assert.equal(checked[0].textContent?.trim(), 'Alpha', '應標在 is_default 那顆')
})
