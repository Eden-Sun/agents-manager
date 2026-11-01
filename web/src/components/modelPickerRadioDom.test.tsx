import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiModelFields, ModelQuickPicker } from './ModelPicker'
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

for (const [configured, runtime] of [[true, false], [false, true], [true, null]] as const) {
  test(`quick Fast menu uses runtime ${runtime}, configured ${configured}, and sends the displayed inverse`, async () => {
    resetStoreForTest()
    const requests = fakeApi((req) => req.method === 'PATCH'
      ? { needs_restart: false, live_apply: { applied: true } }
      : { projects: [] })
    useStore.setState({
      bots: [{ id: 'b-fast', kind: 'codex', identity: null, model: 'alpha', effort: 'low', fast: configured }],
      runs: runtime === null ? {} : { 'b-fast': { id: 'r-fast', bot_id: 'b-fast', state: 'running', runtime_model: 'alpha', runtime_effort: 'low', runtime_fast: runtime } },
      models: { 'codex@local@': [{ ...testModels[0], service_tiers: [{ id: 'priority', name: 'Fast', description: '' }] }] },
      busy: {},
    } as never)
    const root = await mount(<ModelQuickPicker botId="b-fast" kind="codex" host="local">Alpha</ModelQuickPicker>)
    await click(root.querySelector<HTMLButtonElement>('button')!)
    const fast = document.querySelector<HTMLButtonElement>('[role="menuitemcheckbox"]')!
    assert.ok(fast)
    const shown = runtime ?? configured
    assert.equal(fast.getAttribute('aria-checked'), String(shown))
    assert.equal(fast.classList.contains('on'), shown)
    await click(fast)
    await until(() => requests.some((r) => r.method === 'PATCH'), 'Fast PATCH')
    assert.deepEqual(requests.filter((r) => r.method === 'PATCH'), [
      { method: 'PATCH', path: '/api/bots/b-fast', body: { fast: !shown } },
    ], 'including when the requested value already equals bots.fast')
  })
}
