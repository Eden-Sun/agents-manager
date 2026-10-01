/**
 * ConfirmDialog 的互動行為（真的掛進 happy-dom）：
 * - #706 點過框內的非可聚焦文字之後，Esc 仍然關得掉（焦點留在對話框根節點，不是掉到 body）。
 * - 疊層時 Esc 只關事件所在的那一層。
 * - #693 對話框經 portal 掛到 body，但 React 事件仍沿 React 樹冒泡：框裡的點擊不能觸發外層（例如額度卡片列）的 onClick。
 */
import test, { after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, focusLikeBrowserClick, keydown, mount, teardownDom, unmountAll } from '../testing/domHarness'
import { ConfirmDialog } from './ConfirmDialog'

afterEach(unmountAll)
after(teardownDom)

const noop = () => {}

test('#706 點框內文字之後按 Esc：關掉（onCancel 一次、onConfirm 不呼叫），而且點的當下不會關', async () => {
  let cancelled = 0
  let confirmed = 0
  await mount(
    <ConfirmDialog open title="刪除 Bot" body={<p id="txt">這顆 bot 的對話會保留。</p>} confirmLabel="刪除" onConfirm={() => confirmed++} onCancel={() => cancelled++} />,
  )
  const dialog = document.querySelector('[role=alertdialog]')!
  const text = document.getElementById('txt')!
  await click(text)
  assert.equal(cancelled, 0, '點框內不是點背景，不能關')
  focusLikeBrowserClick(text)
  assert.equal(document.activeElement, dialog, '焦點要停在對話框根節點（tabindex=-1），不是掉到 body')
  await keydown(document.activeElement!, 'Escape')
  assert.equal(cancelled, 1)
  assert.equal(confirmed, 0)
})

test('疊層：Esc 只關事件所在的那一層', async () => {
  const calls: string[] = []
  await mount(
    <>
      <ConfirmDialog open title="外層" body="outer" confirmLabel="ok" onConfirm={noop} onCancel={() => calls.push('outer')} />
      <ConfirmDialog open title="內層" body="inner" confirmLabel="ok" onConfirm={noop} onCancel={() => calls.push('inner')} />
    </>,
  )
  const inner = [...document.querySelectorAll('[role=alertdialog]')].find((d) => d.textContent?.includes('內層'))!
  await keydown(inner.querySelector('button')!, 'Escape')
  assert.deepEqual(calls, ['inner'])
})

test('#693 框裡的點擊不會經 React portal 冒泡到外層的 onClick', async () => {
  let outerClicks = 0
  let confirmed = 0
  await mount(
    <div onClick={() => outerClicks++}>
      <ConfirmDialog open title="開 shell 登入" body={<span id="msg">要登入嗎？</span>} confirmLabel="開 shell" onConfirm={() => confirmed++} onCancel={noop} />
    </div>,
  )
  await click(document.getElementById('msg')!)
  assert.equal(outerClicks, 0, '點框裡的文字不能觸發外層（額度卡片列的 toggle 就是這樣被誤觸）')
  const confirmButton = [...document.querySelectorAll('.confirm-actions button')].find((b) => b.textContent === '開 shell')!
  await click(confirmButton)
  assert.equal(confirmed, 1)
  assert.equal(outerClicks, 0, '點確認鈕也不能冒泡')
})
