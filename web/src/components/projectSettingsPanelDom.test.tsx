import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { ProjectSettingsPanel } from './ProjectSettingsPanel'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('專案設定顯示資料與主力列，展開模式設定，執行中的 bot 阻擋刪除', async () => {
  const original = useStore.getState()
  useStore.setState({
    projects: [{ id: 'p', label: '專案甲', path: '/work/p', host: 'local' }] as never,
    bots: [{ id: 'b', name: 'bot-1', kind: 'claude', project_id: 'p', primary: true, primary_position: 1 }] as never,
    runs: { b: { state: 'running' } } as never,
  })
  try {
    await mount(<ProjectSettingsPanel projectId="p" />)
    assert.match(document.body.textContent!, /專案甲/)
    assert.match(document.body.textContent!, /\/work\/p/)
    assert.match(document.body.textContent!, /專案模式設定/)
    // 主力晶片列照樣顯示
    assert.ok(document.querySelector('.unread-bar-wrap'), '應保留主力晶片列')
    // 成員清單已移除
    assert.equal(document.querySelector('.project-settings-members'), null, '不應再有專案成員清單')
    assert.equal(document.querySelector('.project-settings-member'), null, '不應再有成員按鈕')
    // 專案模式設定非 details，直接展開
    assert.equal(document.querySelector('details.project-expert-settings-details'), null, '模式設定不應為 details 收合')
    assert.ok(document.querySelector('.project-expert-settings-section'), '模式設定應直接展開為區塊')

    await click([...document.querySelectorAll('button')].find((b) => b.textContent === '刪除專案…')!)
    assert.match(document.body.textContent!, /仍有 1 個 Bot 在跑/)
    assert.equal([...document.querySelectorAll('button')].find((b) => b.textContent === '刪除 Project')!.disabled, true)
  } finally { await act(async () => useStore.setState(original)) }
})
