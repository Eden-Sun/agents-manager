/**
 * ModelQuickPicker（#939）：claude 的設定存完整 id（`claude-sonnet-5-5`，daemon 把別名展開，#400），選單的 chip 是別名（`sonnet`）。
 * 以前用 `===` 比：一顆都不亮、點同一個模型還會送 `{model:'sonnet'}`（設定被改回別名、pane 被打一次 `/model`），
 * `current` 退回 `is_default`（opus），haiku bot 的「廠推薦」標在 High。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, ModelInfo, PatchBotInput } from '../api/types'
import { ModelQuickPicker } from './ModelPicker'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const model = (id: string, over: Partial<ModelInfo> = {}): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: null,
  efforts: ['low', 'medium', 'high'],
  service_tiers: [],
  ...over,
})

const CLAUDE_MODELS = [
  model('opus', { is_default: true, default_effort: 'high' }),
  model('sonnet', { default_effort: 'high' }),
  model('haiku', { default_effort: 'medium' }),
]

function setup(botModel: string | null, effort: string | null = null) {
  const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', model: botModel, effort, fast: false, identity: null, needs_restart: false } as unknown as Bot
  const patches: PatchBotInput[] = []
  useStore.setState({
    bots: [bot],
    models: { 'claude@local@': CLAUDE_MODELS },
    busy: {},
    patchBot: (async (_id: string, input: PatchBotInput) => {
      patches.push(input)
      return { needsRestart: false }
    }) as never,
  })
  return patches
}

async function openMenu() {
  await mount(<ModelQuickPicker botId="b1" kind="claude" host="local">模型</ModelQuickPicker>)
  const trigger = document.querySelector<HTMLButtonElement>('button[aria-label="改模型"]')
  assert.ok(trigger)
  await click(trigger)
}

const modelItems = () => [...document.querySelectorAll<HTMLButtonElement>('[aria-label="模型"] button[role="menuitemradio"]')]
const checkedModels = () => modelItems().filter((b) => b.getAttribute('aria-checked') === 'true').map((b) => b.textContent)

test('存完整 id 的 claude bot：對應的別名 chip 亮著（aria-checked 與 on）', async () => {
  setup('claude-sonnet-5-5')
  await openMenu()
  assert.deepEqual(checkedModels(), ['sonnet'])
  assert.deepEqual(modelItems().filter((b) => b.classList.contains('on')).map((b) => b.textContent), ['sonnet'])
})

test('點目前已經選的模型（存的是完整 id）：不送 patchBot，只關選單', async () => {
  const patches = setup('claude-sonnet-5-5')
  await openMenu()
  const sonnet = modelItems().find((b) => b.textContent === 'sonnet')
  assert.ok(sonnet)
  await click(sonnet)
  assert.deepEqual(patches, [], '同一個模型不能把設定改回別名')
  assert.equal(document.querySelector('.model-quick-pop'), null, '選單關掉')
})

test('點另一個模型照常送出', async () => {
  const patches = setup('claude-sonnet-5-5')
  await openMenu()
  const haiku = modelItems().find((b) => b.textContent === 'haiku')
  assert.ok(haiku)
  await click(haiku)
  assert.deepEqual(patches, [{ model: 'haiku' }])
})

test('haiku bot（存完整 id）：「廠推薦」標在 haiku 的預設強度 Medium，不是 opus 的 High', async () => {
  setup('claude-haiku-5-5')
  await openMenu()
  const tagged = [...document.querySelectorAll('.effort-recommended')].map((e) => e.closest('button')?.textContent ?? '')
  assert.equal(tagged.length, 1, tagged.join('|'))
  assert.ok(tagged[0].startsWith('Medium'), tagged[0])
})

test('沒指定模型的 bot：預設那顆亮著', async () => {
  setup(null)
  await openMenu()
  assert.deepEqual(checkedModels(), ['opus'])
  assert.deepEqual(modelItems().filter((b) => b.classList.contains('on')).map((b) => b.textContent), ['opus'])
})

test('沒指定強度：模型的預設強度亮著', async () => {
  setup(null)
  await openMenu()
  const checkedEfforts = [...document.querySelectorAll('[aria-label="強度"] button[aria-checked="true"]')]
  assert.equal(checkedEfforts.length, 1)
  assert.ok(checkedEfforts[0].textContent?.startsWith('High'))
})

test('有指定強度就照指定的', async () => {
  setup(null, 'low')
  await openMenu()
  const checkedEfforts = [...document.querySelectorAll('[aria-label="強度"] button[aria-checked="true"]')]
  assert.equal(checkedEfforts.length, 1)
  assert.ok(checkedEfforts[0].textContent?.startsWith('Low'))
})

test('沒指定時點亮著的預設那顆仍會送出', async () => {
  const patches = setup(null)
  await openMenu()
  const opus = modelItems().find((b) => b.textContent === 'opus')
  assert.ok(opus)
  await click(opus)
  assert.deepEqual(patches, [{ model: 'opus' }])
})
