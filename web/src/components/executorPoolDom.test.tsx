import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import type { ExecutorPoolEntry, ModelInfo, PatchProjectInput } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const EFF = ['low', 'medium', 'high', 'xhigh']
const m = (id: string, efforts: string[], context_window: number | null = null, display_name = id): ModelInfo => ({
  id,
  display_name,
  description: '',
  is_default: false,
  default_effort: null,
  efforts,
  context_window,
  service_tiers: [],
})

const MODELS: Record<string, ModelInfo[]> = {
  claude: [m('claude-haiku-4-5', EFF), m('claude-haiku-5-5', EFF), m('claude-sonnet-5-5', ['low', 'medium', 'high', 'max'])],
  codex: [m('gpt-6-luna', EFF, 400000), m('gpt-6.1-terra', EFF)],
  grok: [],
  agy: [m('gemini-3.8-flash-high', []), m('gemini-3.8-flash-low', []), m('gemini-3.8-flash-medium', [])],
}

let patched: PatchProjectInput[] = []
let existing: ExecutorPoolEntry[] = []

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

function button(text: string): HTMLButtonElement {
  const b = Array.from(document.querySelectorAll<HTMLButtonElement>('button')).find((x) => x.textContent?.includes(text))
  assert.ok(b, `應有按鈕「${text}」`)
  return b
}

beforeEach(() => {
  patched = []
  existing = [{ id: 'codex-luna-xhigh-256k', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 2 }]
  fakeApi((req) => {
    const kind = /kind=(\w+)/.exec(req.path)?.[1] ?? ''
    return { kind, host: 'local', models: MODELS[kind] ?? [] }
  })
  useStore.setState({
    models: {},
    modelsFailedAt: {},
    patchProject: async (_id: string, input: PatchProjectInput) => {
      patched.push(input)
      useStore.setState((s) => ({ projects: s.projects.map((p) => (p.id === 'p-expert' ? { ...p, ...input } : p)) }))
      return true
    },
    projects: [{ id: 'p-expert', label: '專家', path: '/e', host: 'local', mode: 'expert', executor_pool: existing }],
    bots: [],
    connected: true,
    defaultConnected: true,
    socket: 'open',
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    busy: {},
  } as never)
})

test('建議：只列出查得到的家族，勾選後才加入，模型取版本最新者', async () => {
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  button('加入建議').click()
  await settle()

  const rows = Array.from(document.querySelectorAll<HTMLLabelElement>('.executor-pool-suggest-row'))
  // sonnet 有清單但 terra／luna／haiku／flash 都有；這組 fixture 沒有 agy 之外缺的家族 → 5 條都在
  assert.equal(rows.length, 5)
  assert.match(rows[0].textContent ?? '', /claude · claude-haiku-5-5 · low · 200k · ×2/)
  assert.match(rows[2].textContent ?? '', /codex · gpt-6\.1-terra · medium · 256k · ×1/)
  assert.match(rows[4].textContent ?? '', /agy · gemini-3\.8-flash-low/)

  // 尚未勾選前按鈕停用、也沒有寫入
  assert.equal(button('加入勾選的建議').disabled, true)
  assert.equal(patched.length, 0)

  const haiku = rows[0].querySelector<HTMLInputElement>('input')!
  await click(haiku)
  await settle()
  await click(button('加入勾選的建議'))
  await settle()

  // 加入後預設建議自動收起，列表多一條；存檔才會送出
  assert.equal(document.querySelectorAll('.executor-pool-entry').length, 2)
  await click(button('儲存'))
  await settle()
  assert.equal(patched.length, 1)
  const pool = patched[0].executor_pool!
  assert.equal(pool.length, 2)
  assert.deepEqual(pool[1], { id: 'claude-haiku-5-5-low-200k', kind: 'claude', model: 'claude-haiku-5-5', effort: 'low', context_tokens: 200000, count: 2 })
})

test('建議：已在池中的組合顯示「已在池中」且不能再勾', async () => {
  existing = [{ id: 'codex-luna-xhigh-256k', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 2 }]
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  button('加入建議').click()
  await settle()
  const rows = Array.from(document.querySelectorAll<HTMLLabelElement>('.executor-pool-suggest-row'))
  const luna = rows.find((r) => r.textContent?.includes('gpt-6-luna'))!
  assert.ok(luna.textContent?.includes('已在池中'))
  assert.equal(luna.querySelector('input')!.disabled, true)
})

test('新增條目：重複的四元組被擋下並顯示錯誤，不寫入池', async () => {
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  button('新增條目').click()
  await settle()

  const form = document.querySelector<HTMLElement>('[data-testid="executor-pool-form"]')!
  const [kindSel, modelSel, effortSel] = Array.from(form.querySelectorAll<HTMLSelectElement>('select'))
  await change(kindSel, 'codex')
  await change(modelSel, 'gpt-6-luna')
  await change(effortSel, 'xhigh')
  const ctxBox = form.querySelector<HTMLInputElement>('input[type="checkbox"]')!
  await click(ctxBox) // 取消「用模型預設」→ 出現拉條，預設 200000（下限）
  await settle()
  const range = form.querySelector<HTMLInputElement>('input[type="range"]')!
  await change(range, '256000', 'input')

  await click(button('加入條目'))
  await settle()
  const err = document.querySelector('[role="alert"]')
  assert.ok(err?.textContent?.includes('已有相同的條目'), '應顯示重複錯誤')
  assert.equal(document.querySelectorAll('.executor-pool-entry').length, 1)
})

test('新增條目：agy 不支援 context，表單的 context 區塊停用', async () => {
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  button('新增條目').click()
  await settle()
  const form = document.querySelector<HTMLElement>('[data-testid="executor-pool-form"]')!
  const kindSel = form.querySelector<HTMLSelectElement>('select')!
  await change(kindSel, 'agy')
  await settle()
  const fieldset = form.querySelector<HTMLFieldSetElement>('fieldset')!
  assert.equal(fieldset.disabled, true)
  assert.ok(form.textContent?.includes('agy 不支援 context 設定'))
})

test('新增條目：送出後以 id 唯一的條目寫入，數量 1..10', async () => {
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  button('新增條目').click()
  await settle()
  const form = document.querySelector<HTMLElement>('[data-testid="executor-pool-form"]')!
  const [kindSel, modelSel] = Array.from(form.querySelectorAll<HTMLSelectElement>('select'))
  await change(kindSel, 'claude')
  await change(modelSel, 'claude-haiku-5-5')
  const count = form.querySelector<HTMLInputElement>('input[type="number"]')!
  await change(count, '99')
  assert.equal(count.value, '10')
  await click(button('加入條目'))
  await settle()
  await click(button('儲存'))
  await settle()
  const pool = patched[0].executor_pool!
  assert.equal(pool.length, 2)
  assert.equal(pool[1].model, 'claude-haiku-5-5')
  assert.equal(pool[1].context_tokens, null)
  assert.equal(pool[1].count, 10)
  assert.notEqual(pool[1].id, pool[0].id)
})
