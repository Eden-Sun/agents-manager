import assert from 'node:assert/strict'
import test, { describe } from 'node:test'
import { closeLeakedShells, sharedMock, virtualMockTime } from './sharedMock'

const shellCount = async () => ((await sharedMock.request('GET', '/hosts/local/shells')) as { shells: unknown[] }).shells.length
const reason = (e: unknown) => (e as { body?: { reason?: string } }).body?.reason

/** 開滿 8 顆，確認第 9 顆（shell 與登入 pane 都一樣）照樣被產品的上限擋下。 */
async function fillToTheLimit() {
  for (let i = 0; i < 8; i++) await sharedMock.request('POST', '/hosts/local/shells', {})
  await assert.rejects(sharedMock.request('POST', '/hosts/local/shells', {}), (e) => reason(e) === 'too_many_shells')
  await assert.rejects(sharedMock.request('POST', '/hosts/local/identities/cc1/login'), (e) => reason(e) === 'too_many_shells')
}

test('closeLeakedShells：把共用 mock 上漏開的 shell 全關掉，之後又能開滿 8 顆、第 9 顆照樣被擋', async () => {
  closeLeakedShells()
  await fillToTheLimit()
  assert.equal(closeLeakedShells(), 8)
  assert.equal(await shellCount(), 0)
  await fillToTheLimit() // 清掉之後上限仍是 8，沒有被放寬
  assert.equal(closeLeakedShells(), 8)
})

// 模擬「前一個測試檔漏關 shell」：這個 describe 就是那個檔，virtualMockTime 的收尾要把殘留清乾淨，
// 緊接著的測試（＝下一個檔）才開得出 8 顆。
describe('前一個檔漏關 shell', () => {
  virtualMockTime()
  test('漏開 8 顆沒關', async () => {
    closeLeakedShells()
    for (let i = 0; i < 8; i++) await sharedMock.request('POST', '/hosts/local/shells', {})
    await sharedMock.request('POST', '/hosts/local/shells', {}).then(() => assert.fail('應該滿了'), () => {})
    assert.equal(await shellCount(), 8)
  })
})

test('前一個檔（virtualMockTime 的收尾）漏的 shell 不會留到下一個檔', async () => {
  assert.equal(await shellCount(), 0)
  await fillToTheLimit()
  closeLeakedShells()
})
