/**
 * #1241：手機的 Git／專案資訊彈窗裡按 issue 的「插入」：插完要通知外層（彈窗收掉），不能把焦點拉回被蓋住的輸入框。
 * 沒有 `onInserted` 的（桌機、群組）照舊把焦點還給輸入框。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useRef } from 'react'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { IssuesBar } from './IssuesBar'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest() // 要在拆 DOM 之前
  await teardownDom()
})

const ISSUE = { number: 12, title: '手機插入後焦點', state: 'open', labels: [], url: 'https://github.com/o/r/issues/12', updated_at: '2026-10-10T00:00:00Z', author: 'a', body_excerpt: '' }

function setupProject() {
  resetStoreForTest()
  fakeApi(() => ({ issues: [ISSUE] }))
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local', github: { owner: 'o', repo: 'r', url: 'https://github.com/o/r' } }] as never,
    drafts: {},
  })
}

function Harness({ onInserted }: { onInserted?: () => void }) {
  const ref = useRef<HTMLTextAreaElement>(null)
  return (
    <>
      <textarea ref={ref} />
      <IssuesBar projectId="p1" draftKey="bot:b1" inputRef={ref} onInserted={onInserted} />
    </>
  )
}

async function openAndInsert() {
  await click(document.querySelector('.issues-btn')!)
  await until(() => document.querySelector('.issues-list .mini-btn') !== null, 'issue 清單載入')
  await click(document.querySelector('.issues-list .mini-btn')!)
}

test('插入後通知外層收掉彈窗，焦點不拉回輸入框', async () => {
  setupProject()
  let calls = 0
  await mount(<Harness onInserted={() => { calls++ }} />)
  await openAndInsert()
  assert.match(useStore.getState().drafts['bot:b1'] ?? '', /#12/, '片段寫進草稿')
  assert.equal(calls, 1, 'onInserted 只叫一次')
  assert.equal(document.querySelector('.issues-pop'), null, 'issue 清單收起來')
  assert.notEqual(document.activeElement, document.querySelector('textarea'), '焦點不被拉回輸入框（手機上那是彈窗外面）')
})

test('沒給 onInserted 時維持原樣：插入後焦點回到輸入框', async () => {
  setupProject()
  await mount(<Harness />)
  await openAndInsert()
  await settle(50)
  assert.equal(document.activeElement, document.querySelector('textarea'))
})
