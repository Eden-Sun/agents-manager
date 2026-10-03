/**
 * 分享 bot（SPEC「分享 bot」）的主 UI：受限 bot 的設定裡才有分享區塊；開 → 拿到完整連結、關掉設定再看只剩末 4 碼；
 * 重產要確認、新連結跟舊的不同；關閉要確認。分享使用者的訊息標「🔗 分享使用者」。mock 的端點規則同契約 B。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { act, click, mockApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { MockTransport } from '../api/mock'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import { BotShareSection } from './BotShareSection'
import { SentViaTag } from './SentViaTag'
import { shareProfileBlocked } from '../lib/shareProfile'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

async function setup() {
  const mock = new MockTransport()
  mockApi(mock)
  const st = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; name: string; share_profile: string | null }[] }[] }
  const bots = st.projects.flatMap((p) => p.bots)
  const shared = bots.find((b) => b.share_profile === 'restricted')!
  const plain = bots.find((b) => b.share_profile === null)!
  await act(async () =>
    useStore.setState({
      bots: bots.map((b) => ({ ...b, project_id: 'p' })),
      notify: () => {},
    } as never),
  )
  return { mock, shared, plain }
}

const btn = (label: string) => [...document.querySelectorAll('button')].find((b) => b.textContent?.trim() === label)!

test('mock 契約：一般 bot 開分享 409 not_shareable；完整 url 只在開／重產時回', async () => {
  const { mock, shared, plain } = await setup()
  await assert.rejects(mock.request('POST', `/bots/${plain.id}/share`, { enabled: true }), (e) => e instanceof ApiError && (e.body as { reason: string }).reason === 'not_shareable')
  const g = (await mock.request('GET', `/bots/${shared.id}/share`)) as { enabled: boolean; url: string | null; token_hint: string }
  assert.equal(g.enabled, true)
  assert.equal(g.url, null, 'GET 不回完整連結')
  assert.match(g.token_hint, /^….{4}$/)
  const r = (await mock.request('POST', `/bots/${shared.id}/share/rotate`)) as { url: string }
  assert.match(r.url, /\/s\/[A-Za-z0-9_-]{43}$/, '32 bytes base64url')
  const off = (await mock.request('POST', `/bots/${shared.id}/share`, { enabled: false })) as { enabled: boolean }
  assert.equal(off.enabled, false)
})

test('一般 bot 的設定裡沒有分享區塊', async () => {
  const { plain } = await setup()
  await mount(<BotShareSection botId={plain.id} />)
  await settle(100)
  assert.equal(document.querySelector('.bs-share'), null)
})

test('受限 bot：已開只看到末 4 碼；重產（確認後）拿到完整新連結；關閉（確認後）', async () => {
  const { shared } = await setup()
  await mount(<BotShareSection botId={shared.id} />)
  await settle(150)
  const sec = document.querySelector('.bs-share')!
  assert.ok(sec, '受限 bot 要有分享區塊')
  assert.equal(sec.querySelector('input[aria-label="分享連結"]'), null, '平常沒有完整連結')
  assert.match(sec.querySelector('.bs-share-hint')!.textContent!, /結尾 …/)
  await click(btn('重產連結'))
  assert.match(document.body.textContent!, /舊連結立刻失效/)
  await click(btn('重產'))
  await settle(150)
  const url = document.querySelector<HTMLInputElement>('input[aria-label="分享連結"]')!.value
  assert.match(url, /^https:\/\/.+\/s\/[A-Za-z0-9_-]{43}$/)
  await click(btn('關閉分享'))
  await click([...document.querySelectorAll('button')].filter((b) => b.textContent?.trim() === '關閉分享').at(-1)!)
  await settle(150)
  assert.equal(document.querySelector('input[aria-label="分享連結"]'), null, '關掉之後不留已失效的連結')
  assert.equal(document.querySelector<HTMLInputElement>('.bs-share-toggle input')!.checked, false)
})

test('分享使用者的訊息標「🔗 分享使用者」，自己發的不標', () => {
  assert.match(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: null, source: 'share' }} />), /🔗 分享使用者/)
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: null, source: 'web' }} />), '')
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'assistant', sent_via: null, source: 'share' }} />), '')
})

test('建 bot 的「分享用（受限）」只給本機的 claude；mock 建 codex 受限 409', async () => {
  assert.equal(shareProfileBlocked('claude', 'local'), null)
  assert.match(shareProfileBlocked('codex', 'local')!, /只支援 claude/)
  assert.match(shareProfileBlocked('claude', 'm4p')!, /本機/)
  const mock = new MockTransport()
  const st = (await mock.request('GET', '/state')) as { projects: { id: string }[] }
  const pid = st.projects[0].id
  await assert.rejects(mock.request('POST', `/projects/${pid}/bots`, { name: 'x-share', kind: 'codex', share_profile: 'restricted', autostart: false }), (e) => e instanceof ApiError && e.status === 409)
  const ok = (await mock.request('POST', `/projects/${pid}/bots`, { name: 'y-share', kind: 'claude', share_profile: 'restricted', autostart: false, auto_approve: true })) as { bot_id: string }
  const st2 = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; share_profile: string | null; auto_approve: boolean }[] }[] }
  const b = st2.projects.flatMap((p) => p.bots).find((x) => x.id === ok.bot_id)!
  assert.equal(b.share_profile, 'restricted')
  assert.equal(b.auto_approve, false, '受限 bot 不帶 bypass permissions')
})
