import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ApiModelFields } from './ModelPicker'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

for (const [kind, want, notWant] of [
  ['claude-api', /下一回合起生效/, /即時套用/],
  ['codex', /即時套用/, /下一回合起生效/],
] as const) {
  test(`${kind}：fast 勾選旁的說明`, async () => {
    useStore.setState({ models: { 'codex@local@': [{ id: 'gpt-x', display_name: 'GPT X', description: '', is_default: true, default_effort: 'high', account_default_effort: null, efforts: ['high'], service_tiers: [{ id: 'priority', name: 'fast', description: 'priority' }] }] } } as never)
    await mount(
      <ApiModelFields kind={kind} host="local" model={null} onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />,
    )
    const row = document.querySelector('.fast-row')
    assert.ok(row, `${kind} 要有 fast 勾選`)
    assert.match(row!.textContent ?? '', want)
    assert.doesNotMatch(row!.textContent ?? '', notWant)
  })
}
