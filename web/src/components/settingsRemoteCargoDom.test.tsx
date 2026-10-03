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
  await typeInto(field('SSH port'), '2222')
  await typeInto(field('SSH 密碼'), 'pw1')
  await click(btn('儲存')!)
  await until(() => sent(requests, 'PUT', /build\/remote/).length === 1, '送出 PUT')
  const body = sent(requests, 'PUT', /build\/remote/)[0].body as Record<string, unknown>
  assert.deepEqual([body.enabled, body.host, body.user, body.ssh_port, body.password], [true, '10.0.0.9', 'ci', 2222, 'pw1'])
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

it('打錯的數字欄位不再靜靜變成預設值：port「2222x」、jobs「abc」會停用儲存／測試並說明（以前 port 悄悄變 22，連到別的埠）', async () => {
  const requests = await open({ enabled: true, host: 'build.example', user: 'builder' })
  await typeInto(field('SSH port'), '2222x')
  await typeInto(field('遠端 cargo jobs'), 'abc')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, true)
  assert.equal((btn('測試連線') as HTMLButtonElement).disabled, true)
  const problems = panel().querySelector('.field-problems')?.textContent ?? ''
  assert.match(problems, /SSH port/)
  assert.match(problems, /cargo jobs/)
  await typeInto(field('SSH port'), '2222')
  await typeInto(field('遠端 cargo jobs'), '8')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, false)
  assert.equal(sent(requests, 'PUT', /build\/remote/).length, 0, '被擋的時候一個請求都沒送')
})

it('daemon 會拒絕的字元（主機含空白、user 以 - 開頭、工作目錄含 ..）前端先擋，不等 400', async () => {
  await open({ enabled: true, host: 'build.example', user: 'builder' })
  await typeInto(field('主機'), 'bad host')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, true)
  assert.match(panel().querySelector('.field-problems')?.textContent ?? '', /主機/)
  await typeInto(field('主機'), 'build.example')
  await typeInto(field('SSH 帳號'), '-oProxyCommand=x')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, true)
  await typeInto(field('SSH 帳號'), 'builder')
  await typeInto(field('遠端工作目錄'), '../etc')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, true)
  await typeInto(field('遠端工作目錄'), 'work/dir')
  assert.equal((btn('儲存') as HTMLButtonElement).disabled, false)
})

it('有沒存的改動時關分頁會被攔（beforeunload）；存了就不攔', async () => {
  await open({ enabled: true, host: 'build.example', user: 'builder' })
  const unload = () => {
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    return ev.defaultPrevented
  }
  assert.equal(unload(), false, '沒改任何東西：不攔')
  await typeInto(field('主機'), 'other.example')
  assert.equal(unload(), true, '改了沒存：攔')
  await click(btn('儲存')!)
  await until(() => message().startsWith('✓ 已儲存'), '存好了')
  assert.equal(unload(), false, '存完不攔')
})

it('新增主機表單：只改進階欄位、名稱與 ssh 還空白時也會攔 beforeunload', async () => {
  await open()
  const form = document.querySelector<HTMLFormElement>('.hosts-panel > form')!
  const unload = () => {
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    return ev.defaultPrevented
  }
  assert.equal(unload(), false, '初始表單沒有未儲存值')
  await click([...form.querySelectorAll('button')].find((b) => b.textContent?.includes('進階'))!)
  const port = [...form.querySelectorAll<HTMLInputElement>('input')].find((i) => i.parentElement?.textContent?.includes('ssh_port'))!
  await typeInto(port, '2223')
  assert.equal(unload(), true, '進階欄位的使用者輸入也不能無提示丟掉')
})

it('外部 Cargo 初始讀取失敗後仍可編輯；未儲存值要攔 beforeunload', async () => {
  const realFetch = globalThis.fetch
  mockApi(mock)
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    if (String(input) === '/api/build/remote' && (init?.method ?? 'GET') === 'GET') {
      return new Response(JSON.stringify({ message: 'read failed' }), { status: 503 })
    }
    return realFetch(input, init)
  }) as unknown as typeof fetch
  try {
    await mount(<HostsPanel />)
    await until(() => document.querySelector('.remote-cargo-settings') !== null, '讀取失敗後仍顯示表單')
    const host = document.querySelector<HTMLInputElement>('.remote-cargo-grid input')!
    await typeInto(host, 'builder.example')
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    assert.equal(ev.defaultPrevented, true, 'saved 為 null 也不能漏掉已編輯的設定')
  } finally {
    globalThis.fetch = realFetch
  }
})

it('新增主機完成時不清掉送出後又輸入的下一筆主機名稱', async () => {
  const requests = await open()
  const originalFetch = globalThis.fetch
  let release!: () => void
  let markStarted!: () => void
  const held = new Promise<void>((resolve) => { release = resolve })
  const started = new Promise<void>((resolve) => { markStarted = resolve })
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    if (String(input) === '/api/hosts' && init?.method === 'POST') {
      markStarted()
      await held
    }
    return originalFetch(input, init)
  }) as unknown as typeof fetch
  try {
    const form = document.querySelector<HTMLFormElement>('.hosts-panel > form')!
    const inputs = [...form.querySelectorAll<HTMLInputElement>('input')]
    const name = inputs.find((i) => i.parentElement?.textContent?.includes('名稱'))!
    const ssh = inputs.find((i) => i.placeholder?.includes('@'))!
    await typeInto(name, 'first-host')
    await typeInto(ssh, 'builder@example.test')
    await click([...form.querySelectorAll('button')].find((b) => b.textContent?.includes('新增並連線'))!)
    await started
    await typeInto(name, 'next-host')
    release()
    await until(() => requests.some((r) => r.method === 'POST' && r.path === '/api/hosts'), '新增主機完成')
    await until(() => document.querySelector('.hosts-panel > .host-result') !== null, '主機結果畫出來')
    assert.equal(name.value, 'next-host', '完成先前請求時不能覆蓋使用者後打的字')
  } finally {
    release()
    globalThis.fetch = originalFetch
  }
})

it('外部 Cargo 儲存完成時不清掉送出後又輸入的新密碼', async () => {
  await open({ enabled: true, host: 'build.example', user: 'builder' })
  const originalFetch = globalThis.fetch
  let release!: () => void
  let markStarted!: () => void
  const held = new Promise<void>((resolve) => { release = resolve })
  const started = new Promise<void>((resolve) => { markStarted = resolve })
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    if (String(input) === '/api/build/remote' && init?.method === 'PUT') {
      markStarted()
      await held
    }
    return originalFetch(input, init)
  }) as unknown as typeof fetch
  try {
    const password = field('SSH 密碼') as HTMLInputElement
    await typeInto(password, 'first-pass')
    await click(btn('儲存')!)
    await started
    await typeInto(password, 'next-pass')
    release()
    await until(() => message().startsWith('✓ 已儲存'), '原密碼儲存完成')
    assert.equal(password.value, 'next-pass', '較晚輸入的新密碼不能被舊儲存回應清空')
    const ev = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(ev)
    assert.equal(ev.defaultPrevented, true, '新密碼仍是未儲存變更')
  } finally {
    release()
    globalThis.fetch = originalFetch
  }
})
