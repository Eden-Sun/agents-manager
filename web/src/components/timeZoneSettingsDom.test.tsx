import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { TimeZoneSettings } from './TimeZoneSettings'
import { useStore } from '../store/store'

afterEach(unmountAll)
before(async () => {
  await setupDom()
})
after(async () => {
  await teardownDom()
})

const text = () => document.body.textContent ?? ''
const saveButton = () => [...document.querySelectorAll('button')].find((b) => b.textContent === '儲存') as HTMLButtonElement

test('時區設定：儲存送 PUT、成功後更新 store 並顯示目前 UTC 偏移', async () => {
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
  const reqs = fakeApi((req) => {
    if (req.method === 'PUT' && req.path.endsWith('/settings/time-zone')) return { time_zone: 'America/New_York', utc_offset: '-5' }
    return { time_zone: 'Asia/Taipei', utc_offset: '+8' }
  })
  await mount(<TimeZoneSettings />)
  await settle()
  assert.match(text(), /目前 UTC\+8/)
  const input = document.querySelector('input[list="time-zone-options"]') as HTMLInputElement
  assert.equal(input.value, 'Asia/Taipei')
  assert.ok(document.querySelector('datalist#time-zone-options option'))
  await typeInto(input, 'America/New_York')
  await click(saveButton())
  await settle()
  const put = reqs.find((r) => r.method === 'PUT')
  assert.deepEqual(put?.body, { time_zone: 'America/New_York' })
  assert.equal(useStore.getState().userTimeZone, 'America/New_York')
  assert.match(text(), /已儲存/)
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
})

test('時區設定：daemon 回 400 bad_time_zone 顯示「時區名稱無效」，store 不變', async () => {
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
  const realFetch = globalThis.fetch
  globalThis.fetch = (async () => new Response(JSON.stringify({ error: 'bad_time_zone' }), { status: 400 })) as unknown as typeof fetch
  try {
    await mount(<TimeZoneSettings />)
    await settle()
    const input = document.querySelector('input[list="time-zone-options"]') as HTMLInputElement
    await typeInto(input, 'Mars/Olympus')
    await click(saveButton())
    await settle()
    assert.match(text(), /時區名稱無效/)
    assert.equal(useStore.getState().userTimeZone, 'Asia/Taipei')
  } finally {
    globalThis.fetch = realFetch
  }
})
