import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, ModelInfo, PatchBotInput } from '../api/types'
import { ApiModelFields, ModelQuickPicker } from './ModelPicker'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const info = (id: string, is_default = false): ModelInfo => ({ id, display_name: id, description: '', is_default, default_effort: null, efforts: [], service_tiers: [] })

const API = [
  info('gemini-3.8-flash-low'),
  info('gemini-3.8-flash-medium', true),
  info('gemini-3.8-flash-high'),
  info('gemini-4-hydrogen-low'),
  info('gemini-4-hydrogen-medium'),
  info('gemini-4-hydrogen-high'),
  info('gemini-4-hydrogen-xhigh'),
  info('gemini-4-argon-low'),
  info('gemini-4-argon-medium'),
  info('gemini-4-argon-high'),
  info('gemini-4-neon-low'),
  info('gemini-4-neon-medium'),
  info('gemini-4-neon-high'),
  info('gemini-4-neon-xhigh'),
  info('gemini-pro-orchestrator-neon'),
  info('gemini-pro-orchestrator-g1-pro'),
]

async function fields(model: string, onModel: (v: string | null) => void, onEffort: (v: string | null) => void = () => {}) {
  await act(() => useStore.setState({ models: { 'agy@local@': API } } as never))
  return mount(<ApiModelFields kind="agy" host="local" model={model} onModel={onModel} effort={null} onEffort={onEffort} fast={false} onFast={() => {}} />)
}

const labels = (el: HTMLElement, group: 'models' | 'efforts') =>
  [...el.querySelectorAll(group === 'models' ? '.opt-group.models .opt' : '.opt-group:not(.models) .opt')].map((b) => b.textContent ?? '')

test('16 個 slug 分成 6 個模型按鈕；Orchestrator 不畫強度列', async () => {
  const picked: (string | null)[] = []
  const el = await fields('gemini-pro-orchestrator-neon', (v) => picked.push(v))
  assert.deepEqual(labels(el, 'models'), ['3.8-Flash', '4-Hydrogen', '4-Argon', '4-Neon', 'Orchestrator Neon', 'Orchestrator Argon'])
  assert.deepEqual(labels(el, 'efforts'), [], 'Orchestrator 沒有強度列')
})

test('切到另一個模型：保留目前強度（新模型有的話）', async () => {
  const picked: (string | null)[] = []
  const el = await fields('gemini-4-neon-xhigh', (v) => picked.push(v))
  assert.deepEqual(labels(el, 'efforts'), ['Low', 'Medium', 'High', 'XHigh'])
  const hydrogen = [...el.querySelectorAll<HTMLElement>('.opt-group.models .opt')].find((b) => b.textContent === '4-Hydrogen')!
  await click(hydrogen)
  assert.deepEqual(picked, ['gemini-4-hydrogen-xhigh'], '4-Hydrogen 有 xhigh → 保留')
})

test('切到沒有目前強度的模型：降為 medium', async () => {
  const picked: (string | null)[] = []
  const el = await fields('gemini-4-neon-xhigh', (v) => picked.push(v))
  const argon = [...el.querySelectorAll<HTMLElement>('.opt-group.models .opt')].find((b) => b.textContent === '4-Argon')!
  await click(argon)
  assert.deepEqual(picked, ['gemini-4-argon-medium'], '4-Argon 沒有 xhigh → medium')
})

test('切到 3.8 Flash（沒有 xhigh）：降為 medium', async () => {
  const picked: (string | null)[] = []
  const el = await fields('gemini-4-hydrogen-xhigh', (v) => picked.push(v))
  const flash = [...el.querySelectorAll<HTMLElement>('.opt-group.models .opt')].find((b) => b.textContent === '3.8-Flash')!
  await click(flash)
  assert.deepEqual(picked, ['gemini-3.8-flash-medium'], '3.8 有 medium')
})

test('點強度只換成該強度的完整 slug，不呼叫 onEffort', async () => {
  const picked: (string | null)[] = []
  const efforts: (string | null)[] = []
  const el = await fields('gemini-4-neon-medium', (v) => picked.push(v), (v) => efforts.push(v))
  const high = [...el.querySelectorAll<HTMLElement>('.opt-group:not(.models) .opt')].find((b) => b.textContent === 'High')!
  await click(high)
  assert.deepEqual(picked, ['gemini-4-neon-high'])
  assert.deepEqual(efforts, [], '不送 effort')
})

test('目前選的強度在按鈕上是 aria-checked；模型按鈕反映所屬群組', async () => {
  const el = await fields('gemini-4-argon-high', () => {})
  const checkedModel = [...el.querySelectorAll('.opt-group.models .opt')].find((b) => b.getAttribute('aria-checked') === 'true')
  assert.equal(checkedModel?.textContent, '4-Argon')
  const checkedEffort = [...el.querySelectorAll('.opt-group:not(.models) .opt')].find((b) => b.getAttribute('aria-checked') === 'true')
  assert.equal(checkedEffort?.textContent, 'High')
})

test('ag1 只有 3.8 Flash：一顆模型按鈕，三個強度；不在清單的模型標在模型按鈕上', async () => {
  await act(() => useStore.setState({ models: { 'agy@local@': API.slice(0, 3) } } as never))
  const el = await mount(<ApiModelFields kind="agy" host="local" model="gemini-4-neon-high" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />)
  assert.deepEqual(labels(el, 'models').map((x) => x.replace(/\s+/g, ' ').trim()), ['3.8-Flash', '4-Neon 不在清單'])
})

function setupBot(botModel: string) {
  const bot = { id: 'a1', name: 'a1', project_id: 'p1', kind: 'agy', model: botModel, effort: null, fast: false, identity: null, needs_restart: false } as unknown as Bot
  const patches: PatchBotInput[] = []
  useStore.setState({
    bots: [bot],
    models: { 'agy@local@': API },
    busy: {},
    patchBot: (async (_id: string, input: PatchBotInput) => {
      patches.push(input)
      return { needsRestart: false }
    }) as never,
  })
  return patches
}

test('快速選單：點新模型送 { model }；點新強度送 { model }（不帶 effort）；點目前的不送', async () => {
  const patches = setupBot('gemini-4-neon-medium')
  await mount(<ModelQuickPicker botId="a1" kind="agy" host="local">快速選單</ModelQuickPicker>)
  await click(document.querySelector<HTMLButtonElement>('button[aria-label="改模型"]')!)
  const pop = document.querySelector('.model-quick-pop')!
  const menu = (text: string) => [...pop.querySelectorAll<HTMLElement>('[role="menuitemradio"]')].find((b) => b.textContent === text)!
  await click(menu('High'))
  assert.deepEqual(patches, [{ model: 'gemini-4-neon-high' }])
})

test('快速選單：點另一個模型送 { model }，不帶 effort；點同一模型不送', async () => {
  const patches = setupBot('gemini-4-neon-medium')
  await mount(<ModelQuickPicker botId="a1" kind="agy" host="local">快速選單</ModelQuickPicker>)
  await click(document.querySelector<HTMLButtonElement>('button[aria-label="改模型"]')!)
  const pop = document.querySelector('.model-quick-pop')!
  const menu = (text: string) => [...pop.querySelectorAll<HTMLElement>('[role="menuitemradio"]')].find((b) => b.textContent === text)!
  await click(menu('4-Argon'))
  assert.deepEqual(patches, [{ model: 'gemini-4-argon-medium' }], '4-Argon 有 medium → 降為 medium')
  assert.ok(patches.every((p) => !('effort' in p)))
})
