import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { ProjectSettingsPanel } from './ProjectSettingsPanel'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('專案設定顯示資料，成員可開對話，執行中的 bot 阻擋刪除', async () => {
  const original = useStore.getState()
  let selected = ''
  useStore.setState({ projects: [{ id: 'p', label: '專案甲', path: '/work/p', host: 'local' }] as never, bots: [{ id: 'b', name: 'bot-1', kind: 'claude', project_id: 'p' }] as never, runs: { b: { state: 'running' } } as never, selectBot: (id) => { selected = id ?? '' } })
  try {
    await mount(<ProjectSettingsPanel projectId="p" />)
    assert.match(document.body.textContent!, /專案甲/)
    assert.match(document.body.textContent!, /\/work\/p/)
    assert.match(document.body.textContent!, /專案模式設定/)
    await click(document.querySelector<HTMLButtonElement>('.project-settings-member')!)
    assert.equal(selected, 'b')
    await click([...document.querySelectorAll('button')].find((b) => b.textContent === '刪除專案…')!)
    assert.match(document.body.textContent!, /仍有 1 個 Bot 在跑/)
    assert.equal([...document.querySelectorAll('button')].find((b) => b.textContent === '刪除 Project')!.disabled, true)
  } finally { await act(async () => useStore.setState(original)) }
})
