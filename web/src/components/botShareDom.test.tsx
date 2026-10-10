/**
 * 分享 bot（SPEC「分享 bot」）的主 UI：受限 bot 的設定裡才有分享區塊，分享中隨時顯示完整連結；
 * 聊天區頂端的分享鈕只給受限 bot（一顆圖示鈕：未分享＝開啟並複製、分享中＝開選單，第一項複製連結，重產／關閉都要確認；#1180）；側欄名字旁 🔗。
 * 重產要確認、新連結跟舊的不同；關閉要確認。分享使用者的訊息標「🔗 分享使用者」。mock 的端點規則同契約 B。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { act, click, fakeApi, keydown, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { MockTransport } from '../api/mock'
import { ApiError } from '../api/types'
import { resetStoreForTest, useStore } from '../store/store'
import { BotShareSection } from './BotShareSection'
import { ShareLinkButton, ShareMark } from './ShareLinkButton'
import { SentViaTag } from './SentViaTag'
import { shareProfileBlocked } from '../lib/shareProfile'

afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

async function setup() {
  const mock = new MockTransport()
  mockApi(mock)
  const st = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; name: string; share_profile: string | null; share_enabled: boolean }[] }[] }
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
const mainBtn = () => document.querySelector<HTMLButtonElement>('.share-link-main')!

test('mock 契約：一般 bot 開分享 409 not_shareable；分享中 GET 回完整 url', async () => {
  const { mock, shared, plain } = await setup()
  await assert.rejects(mock.request('POST', `/bots/${plain.id}/share`, { enabled: true }), (e) => e instanceof ApiError && (e.body as { reason: string }).reason === 'not_shareable')
  const g = (await mock.request('GET', `/bots/${shared.id}/share`)) as { enabled: boolean; url: string | null; token_hint: string; needs_rotate: boolean }
  assert.equal(g.enabled, true)
  assert.match(g.url!, /\/s\/[A-Za-z0-9_-]{43}$/, 'GET 也回完整連結')
  assert.equal(g.needs_rotate, false)
  assert.equal(g.token_hint, `…${g.url!.slice(-4)}`)
  const r = (await mock.request('POST', `/bots/${shared.id}/share/rotate`)) as { url: string }
  assert.match(r.url, /\/s\/[A-Za-z0-9_-]{43}$/, '32 bytes base64url')
  assert.notEqual(r.url, g.url)
  assert.equal(((await mock.request('GET', `/bots/${shared.id}/share`)) as { url: string }).url, r.url, '重產後 GET 回新的那條')
  const off = (await mock.request('POST', `/bots/${shared.id}/share`, { enabled: false })) as { enabled: boolean; url: string | null }
  assert.equal(off.enabled, false)
  assert.equal(off.url, null)
})

test('分享面板測試的 store stub 不會留給下一條測試', () => {
  assert.equal(useStore.getState().notify, useStore.getInitialState().notify)
})

test('一般 bot 的設定裡沒有分享區塊', async () => {
  const { plain } = await setup()
  await mount(<BotShareSection botId={plain.id} />)
  await settle(100)
  assert.equal(document.querySelector('.bs-share'), null)
})

test('受限 bot：分享中隨時看得到完整連結；重產（確認後）換成新連結；關閉（確認後）', async () => {
  const { shared } = await setup()
  await mount(<BotShareSection botId={shared.id} />)
  await settle(150)
  const sec = document.querySelector('.bs-share')!
  assert.ok(sec, '受限 bot 要有分享區塊')
  const before = sec.querySelector<HTMLInputElement>('input[aria-label="分享連結"]')!.value
  assert.match(before, /^https:\/\/.+\/s\/[A-Za-z0-9_-]{43}$/, '一打開設定就有完整連結')
  assert.ok(btn('複製連結'))
  await click(btn('重產連結'))
  assert.match(document.body.textContent!, /舊連結立刻失效/)
  await click(btn('重產'))
  await settle(150)
  const url = document.querySelector<HTMLInputElement>('input[aria-label="分享連結"]')!.value
  assert.match(url, /^https:\/\/.+\/s\/[A-Za-z0-9_-]{43}$/)
  assert.notEqual(url, before)
  await click(btn('關閉分享'))
  await click([...document.querySelectorAll('button')].filter((b) => b.textContent?.trim() === '關閉分享').at(-1)!)
  await settle(150)
  assert.equal(document.querySelector('input[aria-label="分享連結"]'), null, '關掉之後不留已失效的連結')
  assert.equal(document.querySelector<HTMLInputElement>('.bs-share-toggle input')!.checked, false)
})

test('舊資料只有雜湊：設定面板提示重產一次', async () => {
  const { shared } = await setup()
  const { rawTransport } = await import('../api/index')
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (m: string, p: string, b?: unknown) => {
    const r = await orig(m as never, p, b as never)
    return m === 'GET' && p.endsWith('/share') ? { ...(r as object), url: null, needs_rotate: true } : r
  }) as typeof rawTransport.request
  try {
    await mount(<BotShareSection botId={shared.id} />)
    await settle(150)
    assert.equal(document.querySelector('input[aria-label="分享連結"]'), null)
    assert.match(document.querySelector('.bs-share-hint')!.textContent!, /舊版開的.*「重產連結」一次/)
  } finally {
    rawTransport.request = orig
  }
})

test('聊天區頂端分享鈕：一般 bot 沒有；未分享按下＝開啟並複製，分享中＝開選單（第一項複製連結），重產與關閉都要確認', async () => {
  const { mock, shared, plain } = await setup()
  await mount(<ShareLinkButton botId={plain.id} />)
  assert.equal(document.querySelector('.share-link'), null, '一般 bot 沒有分享按鈕')
  await unmountAll()

  await mock.request('POST', `/bots/${shared.id}/share`, { enabled: false })
  const msgs: string[] = []
  const copied: string[] = []
  const nav = navigator as unknown as { clipboard?: unknown }
  const origClip = nav.clipboard
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (t: string) => void copied.push(t) } })
  const origExec = document.execCommand
  document.execCommand = () => false
  const setOn = (on: boolean) => act(async () => useStore.setState((s) => ({ bots: s.bots.map((b) => (b.id === shared.id ? { ...b, share_enabled: on } : b)) })))
  try {
    await act(async () => useStore.setState({ notify: (_k: string, m: string) => void msgs.push(m) } as never))
    await setOn(false)
    await mount(<ShareLinkButton botId={shared.id} />)
    assert.equal(mainBtn().getAttribute('aria-label'), '開啟分享並複製連結')
    assert.equal(mainBtn().textContent, '', '主按鈕只有圖示，沒有文字（#1072）')
    assert.ok(mainBtn().querySelector('svg'), '圖示是 inline SVG，不用 emoji（#1180）')
    assert.equal(mainBtn().getAttribute('aria-haspopup'), null, '沒分享時不是選單鈕')
    await click(mainBtn())
    await settle(50)
    const g = (await mock.request('GET', `/bots/${shared.id}/share`)) as { enabled: boolean; url: string }
    assert.equal(g.enabled, true, '按下就開啟')
    assert.deepEqual(copied, [g.url], '開啟後複製完整連結')
    assert.match(msgs.at(-1)!, /已開啟分享並複製連結/)

    await setOn(true)
    await settle(50)
    assert.equal(document.querySelectorAll('.share-link button').length, 1, '已開啟時標題列仍只有一顆按鈕（#1180）')
    assert.equal(mainBtn().getAttribute('aria-haspopup'), 'menu')
    assert.equal(mainBtn().getAttribute('aria-expanded'), 'false')
    await click(mainBtn())
    await settle(20)
    assert.equal(mainBtn().getAttribute('aria-expanded'), 'true')
    assert.equal(document.querySelector<HTMLElement>('.share-link-menu')!.style.position, 'fixed', '選單 fixed，不被名字列剪掉（#1072）')
    assert.deepEqual([...document.querySelectorAll('.share-link-menu [role="menuitem"]')].map((b) => b.textContent), ['複製連結', '重產連結', '關閉分享'], '第一項是複製連結')
    assert.deepEqual(copied, [g.url], '點鈕只開選單，還沒複製')
    await click(btn('複製連結'))
    await until(() => copied.length === 2, '選單重新讀取後複製連結')
    assert.deepEqual(copied, [g.url, g.url], '選單「複製連結」＝複製同一條')
    assert.equal(document.querySelector('.share-link-menu'), null, '選了就收起選單')

    await click(mainBtn())
    await click(btn('重產連結'))
    assert.match(document.body.textContent!, /舊連結立刻失效/, '重產要確認')
    await click(btn('重產'))
    await settle(50)
    const g2 = (await mock.request('GET', `/bots/${shared.id}/share`)) as { url: string }
    assert.notEqual(g2.url, g.url)
    assert.equal(copied.at(-1), g2.url, '重產後複製新連結')

    await click(mainBtn())
    await click(btn('關閉分享'))
    assert.match(document.body.textContent!, /連結立刻失效/, '關閉要確認')
    await click([...document.querySelectorAll('button')].filter((b) => b.textContent?.trim() === '關閉分享').at(-1)!)
    await settle(50)
    assert.equal(((await mock.request('GET', `/bots/${shared.id}/share`)) as { enabled: boolean }).enabled, false)
  } finally {
    document.execCommand = origExec
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: origClip })
  }
})

test('側欄：分享用 bot 名字旁 🔗，分享中亮起；一般 bot 沒有', async () => {
  const { shared, plain } = await setup()
  assert.equal(shared.share_enabled, true, 'mock 的種子分享開著，/state 照實投影')
  const setOn = (on: boolean) => act(async () => useStore.setState((s) => ({ bots: s.bots.map((b) => (b.id === shared.id ? { ...b, share_enabled: on } : b)) })))
  await setOn(false)
  await mount(<><ShareMark botId={shared.id} /><ShareMark botId={plain.id} /></>)
  const marks = () => [...document.querySelectorAll('.share-mark')]
  assert.equal(marks().length, 1, '一般 bot 不標')
  assert.ok(marks()[0].classList.contains('off'))
  await setOn(true)
  assert.ok(marks()[0].classList.contains('on'), '分享中亮起')
  assert.equal(marks()[0].getAttribute('aria-label'), '分享中')
  assert.equal(marks()[0].textContent, '🔗')
  // 信任分享：🔓、另一個樣式，一眼跟受限的分得出來。
  await act(async () => useStore.setState((s) => ({ bots: s.bots.map((b) => (b.id === shared.id ? { ...b, share_profile: 'trusted' as const } : b)) })))
  assert.equal(marks()[0].textContent, '🔓')
  assert.ok(marks()[0].classList.contains('trusted'))
  assert.equal(marks()[0].getAttribute('aria-label'), '信任分享中')
})

test('分享使用者的訊息標「🔗 分享使用者」，自己發的不標', () => {
  assert.match(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: null, source: 'share' }} />), /🔗 分享使用者/)
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'user', sent_via: null, source: 'web' }} />), '')
  assert.equal(renderToStaticMarkup(<SentViaTag msg={{ role: 'assistant', sent_via: null, source: 'share' }} />), '')
})

test('建 bot 的「分享用（受限）」只給本機的 claude；mock 建 codex 受限 409', async () => {
  assert.equal(shareProfileBlocked('claude'), null)
  assert.match(shareProfileBlocked('codex')!, /只支援 claude/)
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

test('遠端專案也能建分享用 bot：表單不灰掉，新資料夾標在那台的 ~/shared-bots，瀏覽的是那台（DirPicker 帶 host）', async () => {
  assert.equal(shareProfileBlocked('claude'), null)
  const { ShareProfileField } = await import('./ShareProfileField')
  let folder = { mode: 'new' as 'new' | 'existing', name: '', path: '' }
  await mount(<ShareProfileField kind="claude" host="m4p" value="restricted" onChange={() => {}} botName="support" folder={folder} onFolder={(f) => (folder = f)} />)
  const radios = [...document.querySelectorAll<HTMLInputElement>('input[name="share-profile"]')]
  assert.equal(radios[1].disabled, false, '遠端不灰掉受限')
  assert.equal(radios[2].disabled, false, '遠端不灰掉信任分享')
  assert.equal(document.querySelector('.share-folder-prefix')!.textContent, 'm4p：~/shared-bots/')
  await unmountAll()

  const requests = fakeApi(() => ({ path: '/home/m4p/site', parent: '/home/m4p', home: '/home/m4p', entries: [] }))
  folder = { mode: 'existing', name: '', path: '/home/m4p/site' }
  await mount(<ShareProfileField kind="claude" host="m4p" value="restricted" onChange={() => {}} botName="support" folder={folder} onFolder={(f) => (folder = f)} />)
  const browse = [...document.querySelectorAll<HTMLButtonElement>('.share-folder button')].find((b) => b.textContent === '瀏覽…')!
  await click(browse)
  await until(() => requests.some((r) => r.path.includes('/fs/dirs') && r.path.includes('host=m4p')), '瀏覽遠端資料夾要帶 host=m4p')
})

test('建 bot 表單的資料夾：新資料夾預設 bot 名、既有資料夾要絕對路徑並警告 .env 讀得到', async () => {
  const { shareFolderInput } = await import('../lib/shareProfile')
  assert.deepEqual(shareFolderInput({ mode: 'new', name: '', path: '' }, 'support'), { value: { kind: 'new', name: 'support' } })
  assert.deepEqual(shareFolderInput({ mode: 'new', name: 'kefu-1', path: '' }, 'support'), { value: { kind: 'new', name: 'kefu-1' } })
  assert.ok('error' in shareFolderInput({ mode: 'new', name: '.hidden', path: '' }, 'x'))
  assert.ok('error' in shareFolderInput({ mode: 'existing', name: '', path: 'relative/dir' }, 'x'))
  assert.deepEqual(shareFolderInput({ mode: 'existing', name: '', path: ' /srv/docs ' }, 'x'), { value: { kind: 'existing', path: '/srv/docs' } })

  const { ShareProfileField } = await import('./ShareProfileField')
  let folder = { mode: 'existing' as const, name: '', path: '/srv/docs' }
  await mount(<ShareProfileField kind="claude" host="local" value="restricted" onChange={() => {}} botName="support" folder={folder} onFolder={(f) => (folder = f as typeof folder)} />)
  assert.match(document.querySelector('.share-folder-warn')!.textContent!, /所有檔案（含 \.env 之類）/)
  await unmountAll()
  await mount(<ShareProfileField kind="codex" host="local" value="restricted" onChange={() => {}} botName="x" folder={folder} onFolder={() => {}} />)
  assert.equal(document.querySelector('.share-folder'), null, 'codex 不能受限，也就沒有資料夾選項')

  const mock = new MockTransport()
  const st = (await mock.request('GET', '/state')) as { projects: { id: string }[] }
  const pid = st.projects[0].id
  const r = (await mock.request('POST', `/projects/${pid}/bots`, { name: 'docs-share', kind: 'claude', share_profile: 'restricted', share_folder: { kind: 'existing', path: '/srv/docs' }, autostart: false })) as { bot_id: string }
  const st2 = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; cwd: string | null; model: string | null }[] }[] }
  const b = st2.projects.flatMap((p) => p.bots).find((x) => x.id === r.bot_id)!
  assert.equal(b.cwd, '/srv/docs')
  assert.equal(b.model, 'opus', '沒選模型＝最新 Opus（別名交給 CLI 解析）')
})

test('信任分享：選了要在確認框勾「只分享給絕對信任的人」才算數，資料夾預設專案目錄；mock 沒帶 confirm 400', async () => {
  const { ShareProfileField } = await import('./ShareProfileField')
  let value: 'restricted' | 'trusted' | null = null
  let folder = { mode: 'new' as 'new' | 'existing', name: '', path: '' }
  await mount(
    <ShareProfileField
      kind="claude"
      host="local"
      value={value}
      onChange={(v) => (value = v)}
      botName="ops"
      folder={folder}
      onFolder={(f) => (folder = f)}
      projectPath="/home/me/project/app"
    />,
  )
  const radios = [...document.querySelectorAll<HTMLInputElement>('input[name="share-profile"]')]
  assert.deepEqual(radios.map((r) => r.parentElement!.textContent!.trim()), ['不分享', '分享用（受限）', '🔓 信任分享'])
  await click(radios[2])
  const dialog = document.querySelector('[role="alertdialog"]')!
  assert.match(dialog.textContent!, /拿到連結的人可以透過它操作這台機器上的任何東西/)
  const confirmBtn = [...dialog.querySelectorAll('button')].find((b) => b.textContent === '建立信任分享')!
  assert.equal(confirmBtn.disabled, true, '沒勾不能確認')
  assert.equal(value, null)
  await click(dialog.querySelector<HTMLInputElement>('input[type="checkbox"]')!)
  assert.equal(confirmBtn.disabled, false)
  await click(confirmBtn)
  assert.equal(value, 'trusted')
  assert.deepEqual(folder, { mode: 'existing', name: '', path: '/home/me/project/app' }, '資料夾預設專案目錄')

  const mock = new MockTransport()
  const st = (await mock.request('GET', '/state')) as { projects: { id: string }[] }
  const pid = st.projects[0].id
  await assert.rejects(mock.request('POST', `/projects/${pid}/bots`, { name: 't-share', kind: 'claude', share_profile: 'trusted', autostart: false }), (e) => e instanceof ApiError && e.status === 400)
  const ok = (await mock.request('POST', `/projects/${pid}/bots`, { name: 't-share', kind: 'claude', share_profile: 'trusted', confirm_trusted: true, autostart: false, auto_approve: true })) as { bot_id: string }
  const st2 = (await mock.request('GET', '/state')) as { projects: { bots: { id: string; share_profile: string | null; auto_approve: boolean }[] }[] }
  const b = st2.projects.flatMap((p) => p.bots).find((x) => x.id === ok.bot_id)!
  assert.equal(b.share_profile, 'trusted')
  assert.equal(b.auto_approve, true, '信任分享照 bot 設定')
})

test('信任分享：「允許 iframe 嵌入」只在信任分享的分享中畫，勾選送出 allow_embed；受限沒有這個選項', async () => {
  const { mock, shared } = await setup()
  await mount(<BotShareSection botId={shared.id} />)
  await settle(100)
  assert.equal(document.querySelector('.bs-share-embed'), null, '受限不畫')
  await unmountAll()

  const st = (await mock.request('GET', '/state')) as { projects: { id: string }[] }
  const made = (await mock.request('POST', `/projects/${st.projects[0].id}/bots`, {
    name: 'embed-ops',
    kind: 'claude',
    share_profile: 'trusted',
    confirm_trusted: true,
    share_folder: { kind: 'existing', path: '/home/me/site' },
  })) as { bot_id: string }
  await mock.request('POST', `/bots/${made.bot_id}/share`, { enabled: true })
  const bots = ((await mock.request('GET', '/state')) as { projects: { bots: { id: string; name: string; share_profile: string | null; share_enabled: boolean }[] }[] }).projects.flatMap((p) => p.bots)
  await act(async () => useStore.setState({ bots: bots.map((b) => ({ ...b, project_id: 'p' })) } as never))
  const trusted = bots.find((b) => b.id === made.bot_id)!
  await mount(<BotShareSection botId={trusted.id} />)
  await settle(150)
  const box = () => document.querySelector<HTMLInputElement>('.bs-share-embed input')!
  assert.ok(box(), '信任分享畫這個選項')
  assert.match(document.querySelector('.bs-share-embed')!.textContent!, /任何網站都能把這個分享頁嵌進去/)
  assert.equal(box().checked, false, '預設關')
  await click(box())
  await settle(150)
  assert.equal(((await mock.request('GET', `/bots/${trusted.id}/share`)) as { allow_embed: boolean }).allow_embed, true, '勾了送出 allow_embed')
  assert.equal(box().checked, true)
  await click(box())
  await settle(150)
  assert.equal(((await mock.request('GET', `/bots/${trusted.id}/share`)) as { allow_embed: boolean }).allow_embed, false)
})

test('別處重產連結（share_enabled 沒變）：聊天頂端的分享鈕複製的是新連結（#1098）', async () => {
  const { mock, shared } = await setup()
  const copied: string[] = []
  const nav = navigator as unknown as { clipboard?: unknown }
  const origClip = nav.clipboard
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (t: string) => void copied.push(t) } })
  const origExec = document.execCommand
  document.execCommand = () => false
  try {
    await mock.request('POST', `/bots/${shared.id}/share`, { enabled: true })
    await act(async () => useStore.setState((s) => ({ notify: () => {}, bots: s.bots.map((b) => (b.id === shared.id ? { ...b, share_enabled: true } : b)) })) as never)
    await mount(<ShareLinkButton botId={shared.id} />)
    await settle(100)
    await click(mainBtn())
    await click(btn('複製連結'))
    await until(() => copied.length === 1, '初次複製連結')
    const before = copied[0]
    assert.ok(before, '分享中選單的「複製連結」就複製手上那條')

    // 別處（設定面板或另一個分頁）重產：share_enabled 不變，只來一幀 bot_share_changed。
    const r = (await mock.request('POST', `/bots/${shared.id}/share/rotate`)) as { url: string }
    // 重抓是 effect 裡的非同步回寫：整段包在同一個 act 裡，effect 才會在 act 裡跑、回寫才會被 flush 進 DOM。
    // 第二次 store 更新逼 React 在 render 前先 flush 掛著的 effect（notify 是這個元件的 selector）。
    // harness 的 act 環境會吞掉「不在 act 裡」的 fetch 回寫（effect 裡重抓連結的 setShare），DOM 永遠不換。
    // 這一段暫時關掉 act 環境讓 React 照常排程，等重抓回來再還原（finally 也還原）。
    const g = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
    g.IS_REACT_ACT_ENVIRONMENT = false
    try {
      useStore.setState((s) => ({ shareRev: { ...s.shareRev, [shared.id]: (s.shareRev[shared.id] ?? 0) + 1 } }))
      await new Promise((resolve) => setTimeout(resolve, 150))
    } finally {
      g.IS_REACT_ACT_ENVIRONMENT = true
    }

    await click(mainBtn())
    await click(btn('複製連結'))
    await until(() => copied.length === 2, '重產後複製新連結')
    assert.equal(copied.at(-1), r.url, '複製的是重產後的新連結')
    assert.notEqual(copied.at(-1), before)
    await settle(100) // 把非 act 的排程收乾淨，不留給下一條測試
  } finally {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: origClip })
    document.execCommand = origExec
  }
})

test('聊天分享鈕：複製連結重新讀取狀態，不會把別處剛關掉的分享打開（#1187）', async () => {
  const { mock, shared } = await setup()
  await mount(<ShareLinkButton botId={shared.id} />)
  await settle(100)
  const { rawTransport } = await import('../api/index')
  const orig = rawTransport.request.bind(rawTransport)
  const calls: { method: string; path: string }[] = []
  const releaseGets: (() => void)[] = []
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    calls.push({ method, path })
    const result = await orig(method as never, path, body as never)
    if (method === 'GET' && path.endsWith('/share')) {
      return new Promise((resolve) => releaseGets.push(() => resolve(result)))
    }
    return result
  }) as typeof rawTransport.request
  try {
    await mock.request('POST', `/bots/${shared.id}/share`, { enabled: false })
    // 模擬另一個分頁的狀態尚未送到本分頁：UI 仍顯示 enabled=true。
    assert.equal(mainBtn().getAttribute('aria-label'), '複製分享連結')
    await act(async () =>
      useStore.setState((s) => ({ shareRev: { ...s.shareRev, [shared.id]: (s.shareRev[shared.id] ?? 0) + 1 } })),
    )
    await until(() => releaseGets.length === 1, '分享版本變更後的讀取開始')
    await click(mainBtn())
    await click(btn('複製連結'))
    await until(() => releaseGets.length >= 2 || calls.some((c) => c.method === 'POST'), '選單複製期間讀取狀態')
    releaseGets.forEach((release) => release())
    await settle(50)

    assert.equal(calls.filter((c) => c.method === 'POST' && c.path.endsWith('/share')).length, 0, '只複製不能送出開啟分享的 POST')
    assert.equal(((await mock.request('GET', `/bots/${shared.id}/share`)) as { enabled: boolean }).enabled, false, '分享仍保持關閉')
    assert.equal(mainBtn().getAttribute('aria-label'), '開啟分享並複製連結', '本機按鈕狀態也更新為未分享')
  } finally {
    rawTransport.request = orig
  }
})

test('聊天分享鈕：舊版只有 hash 時提示重產，不用 POST 換一條（#1187）', async () => {
  const { shared } = await setup()
  const messages: string[] = []
  await act(async () => useStore.setState({ notify: (_kind: string, message: string) => void messages.push(message) } as never))
  const { rawTransport } = await import('../api/index')
  const orig = rawTransport.request.bind(rawTransport)
  const calls: { method: string; path: string }[] = []
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    calls.push({ method, path })
    const result = await orig(method as never, path, body as never)
    return method === 'GET' && path.endsWith('/share')
      ? { ...(result as object), url: null, needs_rotate: true, token_hint: '…abcd' }
      : result
  }) as typeof rawTransport.request
  try {
    await mount(<ShareLinkButton botId={shared.id} />)
    await settle(50)
    await click(mainBtn())
    await click(btn('複製連結'))
    await until(() => messages.length > 0, '顯示舊版連結提示')

    assert.equal(calls.filter((c) => c.method === 'POST' && c.path.endsWith('/share')).length, 0)
    assert.match(messages.at(-1)!, /舊版開的.*重產連結/)
  } finally {
    rawTransport.request = orig
  }
})

test('分享鈕選單：打開後焦點在第一項，方向鍵移動，Esc 關掉並把焦點還給按鈕（#1198）', async () => {
  const { shared } = await setup()
  await mount(<ShareLinkButton botId={shared.id} />)
  await settle(100)
  await click(mainBtn())
  await settle(50)

  const items = () => [...document.querySelectorAll<HTMLButtonElement>('.share-link-menu [role="menuitem"]')]
  assert.equal(document.activeElement, items()[0], '打開後焦點在第一個選單項目')
  assert.deepEqual(items().map((item) => item.tabIndex), [-1, -1, -1], 'menuitem 不在一般 Tab 順序')
  await keydown(items()[0], 'ArrowDown')
  assert.equal(document.activeElement, items()[1])
  await keydown(items()[1], 'End')
  assert.equal(document.activeElement, items()[2])
  await keydown(items()[2], 'ArrowDown')
  assert.equal(document.activeElement, items()[0], '方向鍵會繞回第一項')
  await keydown(document.activeElement as HTMLElement, 'Escape')
  await settle()
  assert.equal(document.querySelector('.share-link-menu'), null)
  assert.equal(document.activeElement, mainBtn(), 'Esc 關閉後焦點回到分享鈕')
})

test('別處重產連結（share_enabled 沒變）：設定面板換成新連結（#1099）', async () => {
  const { mock, shared } = await setup()
  await mount(<BotShareSection botId={shared.id} />)
  await settle(150)
  const before = document.querySelector<HTMLInputElement>('input[aria-label="分享連結"]')!.value
  assert.ok(before, '分享中就看得到完整連結')

  // 從聊天頂端或另一個分頁重產：share_enabled 不變，只來一幀 bot_share_changed。
  const r = (await mock.request('POST', `/bots/${shared.id}/share/rotate`)) as { url: string }
  // 同 #1098：harness 的 act 環境會吞掉 effect 裡重抓的非 act 回寫，這一段暫時關掉 act 環境，finally 還原。
  const g = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  g.IS_REACT_ACT_ENVIRONMENT = false
  try {
    useStore.setState((s) => ({ shareRev: { ...s.shareRev, [shared.id]: (s.shareRev[shared.id] ?? 0) + 1 } }))
    await new Promise((resolve) => setTimeout(resolve, 150))
  } finally {
    g.IS_REACT_ACT_ENVIRONMENT = true
  }
  await settle(50)
  const after = document.querySelector<HTMLInputElement>('input[aria-label="分享連結"]')!.value
  assert.equal(after, r.url, '輸入框換成新連結')
  assert.notEqual(after, before)
})

test('分享區塊：第一次讀取失敗，連回來自己重抓；讀到之後錯誤收掉、開關可用', async () => {
  const { shared } = await setup()
  let getCalls = 0
  const origFetch = globalThis.fetch
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'GET' && url.includes(`/bots/${shared.id}/share`)) {
      getCalls += 1
      if (getCalls === 1) {
        return new Response(JSON.stringify({ error: 'upstream', message: 'boom' }), { status: 502 })
      }
    }
    return origFetch(input, init)
  }) as typeof fetch

  try {
    useStore.setState({ socket: 'closed' } as never)
    await mount(<BotShareSection botId={shared.id} />)
    await settle(50)
    assert.ok(document.querySelector('.bs-share-error'), '第一次失敗要顯示錯誤訊息')
    const toggle = document.querySelector<HTMLInputElement>('.bs-share-toggle input')!
    assert.equal(toggle.disabled, true, '讀取失敗開關 disabled')
    assert.equal(getCalls, 1, '目前只抓了 1 次')

    await act(async () => {
      useStore.setState({ socket: 'open' } as never)
    })
    await until(() => getCalls === 2, 'GET 重抓第二次')
    await settle(150)
    assert.equal(document.querySelector('.bs-share-error'), null, '讀取成功收掉錯誤訊息')
    assert.equal(toggle.disabled, false, '連線回來重抓成功開關可用')
  } finally {
    globalThis.fetch = origFetch
  }
})

test('讀取一直失敗時不會無限重試', async () => {
  const { shared } = await setup()
  let getCalls = 0
  const origFetch = globalThis.fetch
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'GET' && url.includes(`/bots/${shared.id}/share`)) {
      getCalls += 1
      return new Response(JSON.stringify({ error: 'upstream', message: 'boom' }), { status: 502 })
    }
    return origFetch(input, init)
  }) as typeof fetch

  try {
    useStore.setState({ socket: 'open' } as never)
    await mount(<BotShareSection botId={shared.id} />)
    await settle(300)
    assert.ok(getCalls <= 2, `GET 次數應 <= 2，實際 ${getCalls}`)
  } finally {
    globalThis.fetch = origFetch
  }
})
