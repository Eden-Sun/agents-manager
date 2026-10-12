/**
 * 分享的資料夾欄位驗錯（既有路徑要絕對路徑、新資料夾名稱的規則）：錯誤要關聯到輸入框（aria-invalid＋aria-describedby），
 * 讀屏才會在焦點停在欄位時唸出錯在哪；不能只是欄位底下一段沒有關係的文字。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareProfileField } from './ShareProfileField'
import { EMPTY_SHARE_FOLDER, type ShareFolderDraft } from '../lib/shareProfile'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

async function render(folder: ShareFolderDraft) {
  await mount(
    <ShareProfileField kind="claude" host="local" value="restricted" onChange={() => {}} botName="bot-a" folder={folder} onFolder={() => {}} />,
  )
}

function assertErrorAssociated(input: HTMLInputElement, text: string) {
  assert.equal(input.getAttribute('aria-invalid'), 'true', '驗錯的欄位要標 aria-invalid')
  const id = input.getAttribute('aria-describedby')
  const err = id ? document.getElementById(id) : null
  assert.equal(err?.textContent?.trim(), text, 'aria-describedby 要指到錯誤文字')
}

test('既有資料夾路徑不是絕對路徑：路徑欄標 aria-invalid，錯誤文字關聯到它', async () => {
  await render({ ...EMPTY_SHARE_FOLDER, mode: 'existing', path: 'relative/dir' })
  const input = document.querySelector<HTMLInputElement>('input[aria-label="既有資料夾路徑"]')!
  assertErrorAssociated(input, '既有資料夾要給絕對路徑（/ 開頭），或按「瀏覽…」挑一個')
})

test('新資料夾名稱不合規：名稱欄標 aria-invalid，錯誤文字關聯到它', async () => {
  await render({ ...EMPTY_SHARE_FOLDER, mode: 'new', name: 'bad name!' })
  const input = document.querySelector<HTMLInputElement>('input[aria-label="新資料夾名稱"]')!
  assertErrorAssociated(input, '資料夾名稱只能用英數與 . _ -，英數開頭，最多 64 字')
})

test('路徑合法：不標 aria-invalid', async () => {
  await render({ ...EMPTY_SHARE_FOLDER, mode: 'existing', path: '/home/you/docs' })
  const input = document.querySelector<HTMLInputElement>('input[aria-label="既有資料夾路徑"]')!
  assert.equal(input.hasAttribute('aria-invalid'), false)
  assert.equal(input.hasAttribute('aria-describedby'), false)
})
