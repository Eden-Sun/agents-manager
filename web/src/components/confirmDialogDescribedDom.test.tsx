/**
 * 確認框（role=alertdialog）的說明文字要關聯到對話框：打開時讀屏要唸出「會發生什麼」，不能只唸標題。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ConfirmDialog } from './ConfirmDialog'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('確認框的說明文字關聯到 alertdialog：aria-describedby 指到 body', async () => {
  await mount(
    <ConfirmDialog open title="這是 AGM 的 Bot" body={<p>刪了可能會壞：排好的例行工作會找不到這顆。</p>} confirmLabel="仍要刪除" onConfirm={() => {}} onCancel={() => {}} />,
  )
  const dialog = document.querySelector<HTMLElement>('[role="alertdialog"]')!
  assert.ok(dialog, '要畫出 alertdialog')
  const describedBy = dialog.getAttribute('aria-describedby')
  const described = describedBy ? document.getElementById(describedBy) : null
  assert.ok(described?.textContent?.includes('刪了可能會壞'), '說明文字要是對話框的 description')
  assert.equal(dialog.getAttribute('aria-labelledby') !== null && document.getElementById(dialog.getAttribute('aria-labelledby')!)?.textContent, '這是 AGM 的 Bot')
})
