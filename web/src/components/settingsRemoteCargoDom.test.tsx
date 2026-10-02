/**
 * 環境設定「主機」頁底下的外部 Cargo 編譯主機（RemoteCargoPanel）：讀回已存設定、儲存、測試連線（測的是**已存**的，
 * 表單有沒存的改動要先存）、缺 cargo／clippy 時的安裝、密碼的「沿用／換新／清掉」。真的掛進 happy-dom，後端是 mock。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { HostsPanel } from './HostsPanel'

const mock = sharedMock

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const panel = () => document.querySelector('.remote-cargo-settings')!
const field = (label: string) =>
  [...panel().querySelectorAll('label.field')].find((l) => l.querySelector('span')?.textContent?.startsWith(label))!.querySelector('input')!
const btn = (text: string) => [...panel().querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const message = () => panel().querySelector('pre.host-result')?.textContent ?? ''
const sent = (requests: FakeRequest[], method: string, re: RegExp) => requests.filter((r) => r.method === method && re.test(r.path))

/** 先把 daemon 端存成已知的樣子，再掛面板。 */
async function open(saved: Record<string, unknown> = {}) {
  await mock.request('PUT', '/build/remote', { enabled: false, host: '', user: '', ssh_port: 22, remote_root: '', cargo_jobs: 4, password: '', ...saved })
  const requests = mockApi(mock)
  await mount(<HostsPanel />)
  await until(() => document.querySelector('.remote-cargo-settings') !== null, '面板讀完設定')
  return requests
}

it('讀回已存的設定到表單；密碼不回傳，只顯示「已安全儲存」', async () => {
  await open({ enabled: true, host: 'build.example', user: 'builder', ssh_port: 2222, cargo_jobs: 6, password: 'hunter2' })
  assert.equal(field('主機').value, 'build.example')
  assert.equal(field('SSH 帳號').value, 'builder')
  assert.equal(field('SSH port').value, '2222')
  assert.equal(field('遠端 cargo jobs').value, '6')
  assert.match(field('SSH 密碼').placeholder, /已安全儲存/)
  assert.equal(field('SSH 密碼').value, '')
  assert.doesNotMatch(document.body.innerHTML, /hunter2/)
})

it('啟用卻沒填主機／帳號：儲存鍵停用；填好後儲存，body 是收斂過的值，密碼欄清空', async () => {
  const requests = await open()
  const enable = panel().querySelector<HTMLInputElement>('input[type=checkbox]')!
  await click(enable)
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, true)
  await typeInto(field('主機'), '  10.0.0.9 ')
  await typeInto(field('SSH 帳號'), 'ci')
  await typeInto(field('SSH port'), 'abc')
  await typeInto(field('SSH 密碼'), 'pw1')
  await click(btn('儲存')!)
  await until(() => sent(requests, 'PUT', /build\/remote/).length === 1, '送出 PUT')
  const body = sent(requests, 'PUT', /build\/remote/)[0].body as Record<string, unknown>
  assert.deepEqual([body.enabled, body.host, body.user, body.ssh_port, body.password], [true, '10.0.0.9', 'ci', 22, 'pw1'])
  await until(() => message().startsWith('✓ 已儲存'), '已儲存訊息')
  assert.equal(field('SSH 密碼').value, '', '密碼欄清空')
  assert.match(field('SSH 密碼').placeholder, /已安全儲存/, '之後顯示已存密碼')
})

it('測試連線：表單有沒存的改動就先存再測（順序 PUT→test）；沒改動就直接測、不多存', async () => {
  const requests = await open({ enabled: true, host: 'build.example', user: 'builder' })
  await click(btn('測試連線')!)
  await until(() => sent(requests, 'POST', /build\/remote\/test/).length === 1, '直接測')
  assert.equal(sent(requests, 'PUT', /build\/remote/).length, 0, '沒改動不能多存')
  await until(() => /SSH\/Cargo 可用/.test(message()), '可用訊息')
  assert.doesNotMatch(message(), /已先儲存/)

  await typeInto(field('主機'), 'other.example')
  await click(btn('測試連線')!)
  await until(() => sent(requests, 'POST', /build\/remote\/test/).length === 2, '第二次測')
  const order = requests.filter((r) => /build\/remote/.test(r.path) && r.method !== 'GET').map((r) => r.method)
  assert.deepEqual(order, ['POST', 'PUT', 'POST'], '改了主機：先存、再測')
  await until(() => /已先儲存表單裡的設定/.test(message()), '訊息說明已先存')
  assert.match(message(), /builder@other\.example/)
})

it('連得上但沒有 cargo：出現「安裝 Rust 工具鏈」，裝完訊息更新、按鈕消失', async () => {
  const requests = await open({ enabled: true, host: 'nocargo.example', user: 'builder' })
  assert.equal(btn('安裝 Rust 工具鏈'), undefined, '測試前不顯示')
  await click(btn('測試連線')!)
  await until(() => btn('安裝 Rust 工具鏈') !== undefined, '缺 cargo 時出現安裝鍵')
  assert.match(message(), /還沒有 Rust 工具鏈/)
  await click(btn('安裝 Rust 工具鏈')!)
  await until(() => sent(requests, 'POST', /install-toolchain/).length === 1, '打了 install-toolchain')
  await until(() => /已安裝：cargo 1\.90\.0/.test(message()), '安裝完成訊息')
  assert.equal(btn('安裝 Rust 工具鏈'), undefined)
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, false, 'busy 放掉')
})

it('測試連線失敗（502）：訊息講原因、不出現安裝鍵、按鈕回到可按', async () => {
  await open({ enabled: true, host: 'unreach.example', user: 'builder' })
  await click(btn('測試連線')!)
  await until(() => message().startsWith('測試失敗：'), '失敗訊息')
  assert.match(message(), /Operation timed out/)
  assert.equal(btn('安裝 Rust 工具鏈'), undefined)
  assert.equal((btn('測試連線') as HTMLButtonElement).disabled, false)
})

it('已存密碼：出現「清除」勾選，勾了儲存送 password:""；沒勾、沒打就不帶 password（沿用）', async () => {
  const requests = await open({ enabled: true, host: 'build.example', user: 'builder', password: 'old' })
  await click(btn('儲存')!)
  await until(() => sent(requests, 'PUT', /build\/remote/).length === 1, '第一次儲存')
  assert.equal('password' in (sent(requests, 'PUT', /build\/remote/)[0].body as object), false, '沒動密碼就不帶')
  await until(() => message().startsWith('✓ 已儲存'), '第一次儲存完成（busy 放掉）')
  const clear = [...panel().querySelectorAll<HTMLInputElement>('input[type=checkbox]')].find((c) => c.parentElement?.textContent?.includes('清除已存密碼'))!
  await click(clear)
  await click(btn('儲存')!)
  await until(() => sent(requests, 'PUT', /build\/remote/).length === 2, '第二次儲存')
  assert.equal((sent(requests, 'PUT', /build\/remote/)[1].body as { password: string }).password, '')
  await until(() => /未設定/.test(field('SSH 密碼').placeholder), '清掉後顯示未設定')
  assert.equal([...panel().querySelectorAll('input[type=checkbox]')].some((c) => c.parentElement?.textContent?.includes('清除已存密碼')), false, '沒密碼就沒有清除勾選')
})
