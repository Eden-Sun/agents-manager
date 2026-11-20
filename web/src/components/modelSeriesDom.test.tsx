import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiModelFields, ModelQuickPicker } from './ModelPicker'
import type { Bot, ModelInfo } from '../api/types'
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
  assert.deepEqual(texts, ['6-Luna', '6.1-Terra', '6.2-Sol', '6.2-Astra'])
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

test('目前模型不在清單仍顯示且選中（Bot 設定、建立表單、快速改模型選單）與 stale 提示', async () => {
  // 1. Bot 設定 (ApiModelFields with agy, API failed so models is null -> static fallback)
  await act(() => useStore.setState({ models: { 'agy@local@': null }, modelsFailedAt: { 'agy@local@': Date.now() } } as never))
  const settingEl = await mount(
    <ApiModelFields kind="agy" host="local" model="gemini-4-neon-medium" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const radios = [...settingEl.querySelectorAll<HTMLElement>('[role="radio"]')]
  const neonRadio = radios.find((r) => r.getAttribute('title')?.startsWith('gemini-4-neon-medium'))
  assert.ok(neonRadio, '目前模型 gemini-4-neon-medium 必須在選項中')
  assert.equal(neonRadio.getAttribute('aria-checked'), 'true', '目前模型必須被選中')
  assert.ok(neonRadio.textContent?.includes('4-Neon Medium'), '標籤應為 4-Neon Medium')
  assert.ok(neonRadio.textContent?.includes(t('不在清單')), '應標註不在清單')
  // stale 提示
  const warnNote = settingEl.querySelector('.field-note.warn')
  assert.ok(warnNote, '應有警示小提示')
  assert.match(warnNote.textContent ?? '', /此主機的模型清單暫時讀不到，顯示的是備用清單/)

  await unmountAll()

  // 2. 建立 bot 表單 (ApiModelFields with preselected model not in static list)
  const createEl = await mount(
    <ApiModelFields kind="agy" host="local" model="gemini-4-hydrogen-xhigh" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const createRadios = [...createEl.querySelectorAll<HTMLElement>('[role="radio"]')]
  const hydrogenRadio = createRadios.find((r) => r.getAttribute('title')?.startsWith('gemini-4-hydrogen-xhigh'))
  assert.ok(hydrogenRadio, '建立表單預填模型必須在選項中')
  assert.equal(hydrogenRadio.getAttribute('aria-checked'), 'true', '建立表單預填模型必須被選中')
  assert.ok(hydrogenRadio.textContent?.includes('4-Hydrogen XHigh'), '標籤應為 4-Hydrogen XHigh')
  assert.ok(hydrogenRadio.textContent?.includes(t('不在清單')), '應標註不在清單')

  await unmountAll()

  // 3. 快速改模型選單 (ModelQuickPicker)
  const bot = { id: 'b-agy-1', name: 'agy-bot', project_id: 'p1', kind: 'agy', model: 'gemini-4-neon-medium', effort: null, fast: false, identity: null, needs_restart: false } as unknown as Bot
  useStore.setState({
    bots: [bot],
    models: { 'agy@local@': null },
    modelsFailedAt: { 'agy@local@': Date.now() },
    busy: {},
  })
  await mount(<ModelQuickPicker botId="b-agy-1" kind="agy" host="local">快速選單</ModelQuickPicker>)
  const trigger = document.querySelector<HTMLButtonElement>('button[aria-label="改模型"]')
  assert.ok(trigger)
  await click(trigger)
  const popover = document.querySelector('.model-quick-pop')
  assert.ok(popover, '快速選單彈出框應打開')
  const menuRadios = [...popover.querySelectorAll<HTMLElement>('[role="menuitemradio"]')]
  const menuNeon = menuRadios.find((r) => r.getAttribute('title')?.startsWith('gemini-4-neon-medium'))
  assert.ok(menuNeon, '快速選單中必須出現 gemini-4-neon-medium')
  assert.equal(menuNeon.getAttribute('aria-checked'), 'true', '快速選單中必須被選中')
  assert.ok(menuNeon.textContent?.includes('4-Neon Medium'), '標籤應為 4-Neon Medium')
  assert.ok(menuNeon.textContent?.includes(t('不在清單')), '快速選單中應標註不在清單')
  // 快速選單中的 stale 提示
  const popWarn = popover.querySelector('.hint.field-note.warn')
  assert.ok(popWarn, '快速選單應有 stale 提示')
  assert.match(popWarn.textContent ?? '', /此主機的模型清單暫時讀不到，顯示的是備用清單/)
})

test('模型清單來源 stale 提示：other-host 沿用其他主機清單、static 顯示備用清單、claude 正常 static 不顯示提示', async () => {
  // 1. other-host 且 source_host 為 local
  await act(() =>
    useStore.setState({
      models: { 'agy@m4p@': [model('gemini-3.8-flash-medium')] },
      modelsMeta: { 'agy@m4p@': { source: 'other-host', source_host: 'local', stale: true } },
    } as never),
  )
  const el1 = await mount(
    <ApiModelFields kind="agy" host="m4p" model="gemini-3.8-flash-medium" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const warn1 = el1.querySelector('.field-note.warn')
  assert.ok(warn1, 'other-host local 應有提示')
  assert.match(warn1.textContent ?? '', /此主機的模型清單暫時讀不到，沿用 本機 的清單/)
  await unmountAll()

  // 2. other-host 且 source_host 為遠端主機
  await act(() =>
    useStore.setState({
      models: { 'agy@m4p@': [model('gemini-3.8-flash-medium')] },
      modelsMeta: { 'agy@m4p@': { source: 'other-host', source_host: 'remote-1', stale: true } },
    } as never),
  )
  const el2 = await mount(
    <ApiModelFields kind="agy" host="m4p" model="gemini-3.8-flash-medium" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const warn2 = el2.querySelector('.field-note.warn')
  assert.ok(warn2, 'other-host remote-1 應有提示')
  assert.match(warn2.textContent ?? '', /此主機的模型清單暫時讀不到，沿用 remote-1 的清單/)
  await unmountAll()

  // 3. static 回傳（agy 抓不到退回 static）
  await act(() =>
    useStore.setState({
      models: { 'agy@m4p@': [model('gemini-3.8-flash-medium')] },
      modelsMeta: { 'agy@m4p@': { source: 'static' } },
    } as never),
  )
  const el3 = await mount(
    <ApiModelFields kind="agy" host="m4p" model="gemini-3.8-flash-medium" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  const warn3 = el3.querySelector('.field-note.warn')
  assert.ok(warn3, 'agy static 應有備用清單提示')
  assert.match(warn3.textContent ?? '', /此主機的模型清單暫時讀不到，顯示的是備用清單/)
  await unmountAll()

  // 4. claude 正常 static 不應顯示警告
  await act(() =>
    useStore.setState({
      models: { 'claude@local@': [model('claude-sonnet-4-6')] },
      modelsMeta: { 'claude@local@': { source: 'static' } },
    } as never),
  )
  const el4 = await mount(
    <ApiModelFields kind="claude" host="local" model="claude-sonnet-4-6" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
  )
  assert.equal(el4.querySelector('.field-note.warn'), null, 'claude static 正常不應有警告')
  await unmountAll()
})
