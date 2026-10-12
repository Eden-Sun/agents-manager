/**
 * 重置時間讀不出來（daemon 給了不是時間的字串）：倒數與刻度的提示不能寫出「NaN」。
 * 照額度列既有的空狀態：沒有可算的倒數就顯示「—」、不掛提示。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const BAD = 'not-a-date'

async function mountWithBadReset(credits: unknown = null) {
  resetStoreForTest()
  useStore.setState({
    quota: {
      claude: {
        five_hour: { used_pct: 40, low: false, critical: false, resets_at: BAD, observed_at: null },
        seven_day: null,
        fable: { used_pct: 20, low: false, critical: false, resets_at: BAD, observed_at: null },
        reset_credits: credits,
        limit_hit: null,
        plan: null,
        updated_at: new Date().toISOString(),
        stale: false,
        host: 'local',
      },
    },
    identities: [],
  } as never)
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.claude') !== null, 'Claude 額度格畫出來')
  return document.querySelector<HTMLElement>('.quota-hp.claude')!
}

/** 格子裡所有文字與 title／aria-label，任何一處含 NaN 都算。 */
function nanIn(el: Element): string[] {
  const hits: string[] = []
  for (const n of [el, ...el.querySelectorAll('*')]) {
    for (const attr of ['title', 'aria-label']) {
      const v = n.getAttribute(attr)
      if (v?.includes('NaN')) hits.push(`${n.tagName}[${attr}]=${v}`)
    }
  }
  if (el.textContent?.includes('NaN')) hits.push(`text=${el.textContent}`)
  return hits
}

test('重置時間不是時間：格子與刻度的提示不寫 NaN', async () => {
  const cell = await mountWithBadReset()
  assert.deepEqual(nanIn(cell), [])
})

test('重置時間不是時間：popover 的「還有多久」顯示「—」，不寫 NaN', async () => {
  const cell = await mountWithBadReset()
  await click(cell.querySelector('.quota-bars-open')!)
  const pop = document.querySelector<HTMLElement>('.quota-pop')!
  assert.deepEqual(nanIn(pop), [])
  const resets = [...pop.querySelectorAll<HTMLElement>('.quota-reset')].map((el) => el.textContent?.replace('↻', '').trim())
  assert.ok(resets.length > 0, 'popover 要有重置那一欄')
  assert.ok(resets.every((t) => t === '—'), `讀不出來就該是「—」，實際 ${JSON.stringify(resets)}`)
})

test('重置券的過期時間讀不出來：提示不寫 NaN', async () => {
  const cell = await mountWithBadReset({ available: 1, expires_at: BAD, title: '券' })
  await click(cell.querySelector('.quota-bars-open')!)
  assert.deepEqual(nanIn(document.querySelector<HTMLElement>('.quota-pop')!), [])
})
