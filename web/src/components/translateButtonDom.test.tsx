/**
 * 對話裡的「翻譯」鈕：英文 assistant 訊息有、中文沒有；點了打 daemon，譯文顯示在原文下方，可切換顯示。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { resetTranslationStore } from '../lib/messageTranslationStore'
import { useMessageTranslation } from './useMessageTranslation'
import { resetTranslateReady } from './useTranslateReady'

before(async () => {
  await setupDom()
  // 同一個程序裡前面的測試檔可能已經掛過英文訊息、把「未啟用」的讀取結果留在模組快取：從乾淨的開始。
  resetTranslateReady()
  resetTranslationStore()
})
afterEach(async () => {
  await unmountAll()
  resetTranslateReady()
  resetTranslationStore()
  localStorage.removeItem('am.msgTranslate.want')
})
after(teardownDom)

const msg = (content: string, over: Partial<Message> = {}): Message => ({
  id: 'm1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-04T00:00:00Z', ...over,
})

function Probe({ m }: { m: Message }) {
  const { button, content, animClass, swapKey } = useMessageTranslation(m)
  return (
    <div>
      <div className="meta">{button}</div>
      <div className="bubble">
        <div key={swapKey} className={animClass}>
          {content ?? m.content}
        </div>
      </div>
    </div>
  )
}

const SETTINGS_ON = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true }] }

const EN = 'I fixed the bug in the parser and added a regression test for it.'

test('英文訊息有「翻譯」鈕，中文與 user 訊息沒有', async () => {
  fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯')
  await unmountAll()
  await mount(<Probe m={msg('已經修好了，請重新整理頁面再試一次。')} />)
  assert.equal(document.querySelector('.msg-translate'), null)
  await unmountAll()
  await mount(<Probe m={msg(EN, { role: 'user' })} />)
  assert.equal(document.querySelector('.msg-translate'), null)
})

test('點了：打 POST /messages/:id/translate，譯文在同一個泡泡裡翻面取代原文，按鈕切回原文', async () => {
  const reqs = fakeApi((r) => (r.path.split('?')[0].endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  const bubble = () => document.querySelector('.bubble')!
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  assert.ok(reqs.some((r) => r.method === 'POST' && r.path.includes('/messages/m1/translate')))
  assert.match(bubble().textContent ?? '', /我修好了解析器的 bug/)
  assert.doesNotMatch(bubble().textContent ?? '', /parser/, '原文被譯文取代，不是疊在下面')
  assert.equal(document.querySelector('.msg-translation-cost')!.textContent, '(by Haiku $0.00042)')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 原文')
  await click(document.querySelector('.msg-translate')!)
  assert.equal(document.querySelector('.msg-translation'), null)
  assert.match(bubble().textContent ?? '', /parser/)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 譯文')
  const before = reqs.length
  await click(document.querySelector('.msg-translate')!)
  assert.ok(document.querySelector('.msg-translation'))
  assert.equal(reqs.length, before, '切換不再打 API')
})

test('低分譯文（low_confidence）照顯示譯文，旁邊標「品質分數偏低（Jev 0.42）」', async () => {
  fakeApi((r) => (r.path.split('?')[0].endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042, low_confidence: true, jev_prob: 0.42 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  assert.match(document.querySelector('.bubble')?.textContent ?? '', /我修好了解析器的 bug/, '低分仍顯示譯文')
  assert.equal(document.querySelector('.msg-translation-lowconf')?.textContent, '品質分數偏低（Jev 0.42）')
})

test('一般譯文沒有低分標籤', async () => {
  fakeApi(TR_ROUTE)
  await mount(<Probe m={msg(EN)} />)
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  assert.equal(document.querySelector('.msg-translation-lowconf'), null)
})

test('未啟用：按鈕事先標「翻譯（未啟用）」，按了只提示、不打翻譯 API', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? { ...SETTINGS_ON, uses: [{ id: 'message_translate', enabled: false }] } : {}))
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translate.off') !== null, '標成未啟用')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯（未啟用）')
  assert.match(document.querySelector('.msg-translate')?.getAttribute('title') ?? '', /對話訊息翻譯/)
  await click(document.querySelector('.msg-translate')!)
  assert.ok(!reqs.some((r) => r.path.includes('/translate')), '沒有打翻譯 API')
})

// 使用者 2026-11-04：「點過翻譯不要自動換回原文，維持譯文優先，除非 user 再次 click」
const TR_ROUTE = (r: { path: string }) => (r.path.endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {})
const translateCalls = (reqs: { method: string; path: string }[]) => reqs.filter((r) => r.method === 'POST' && r.path.includes('/translate')).length

test('點翻譯 → unmount → 重新 mount 仍是譯文，沒有第二次 API 呼叫', async () => {
  const reqs = fakeApi(TR_ROUTE)
  await mount(<Probe m={msg(EN)} />)
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  await unmountAll()
  await mount(<Probe m={msg(EN)} />)
  assert.ok(document.querySelector('.msg-translation'), 'remount 後仍是譯文')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 原文')
  assert.equal(translateCalls(reqs), 1)
})

test('雙向切換：手動換面原→譯帶 msg-swap-forward 且圖示 spin-forward，譯→原帶 msg-swap-backward 且圖示 spin-backward', async () => {
  fakeApi((r) => (r.path.includes('/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  // 原→譯
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  const fwdBody = document.querySelector('.msg-swap-forward')
  assert.ok(fwdBody, '原→譯應有 msg-swap-forward 動畫 class')
  const fwdIco = document.querySelector('.msg-translate-ico.spin-forward')
  assert.ok(fwdIco, '原→譯應有 spin-forward 圖示旋轉 class')

  // 譯→原
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') === null, '切回原文')
  const bwdBody = document.querySelector('.msg-swap-backward')
  assert.ok(bwdBody, '譯→原應有 msg-swap-backward 動畫 class')
  const bwdIco = document.querySelector('.msg-translate-ico.spin-backward')
  assert.ok(bwdIco, '譯→原應有 spin-backward 圖示旋轉 class')
})

test('快速連點 3 下：以最後一次為準停在譯文，動畫為 msg-swap-forward，不另打翻譯 API', async () => {
  const reqs = fakeApi((r) => (r.path.includes('/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  const btn = document.querySelector<HTMLElement>('.msg-translate')!
  // 第一次點擊：打 API 並切到譯文
  await click(btn)
  await until(() => document.querySelector('.msg-translation') !== null, '第一次譯文顯示')
  assert.equal(translateCalls(reqs), 1)

  // 快速連點：譯→原、原→譯、譯→原、原→譯
  await click(btn)
  await click(btn)
  await click(btn)
  await click(btn)
  assert.ok(document.querySelector('.msg-translation'), '連點後以最後一次為準停在譯文')
  assert.ok(document.querySelector('.msg-swap-forward'), '最後是原→譯，應有 msg-swap-forward')
  assert.ok(document.querySelector('.msg-translate-ico.spin-forward'), '圖示為 spin-forward')
  assert.equal(translateCalls(reqs), 1, '快速連點不另發翻譯請求')
})

test('重掛不播動畫：初次掛載、偏好恢復與 remount 不帶 msg-swap-* 與 spin-* 動畫 class', async () => {
  fakeApi((r) => (r.path.includes('/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  await unmountAll()

  // 模擬虛擬列重掛／切換 bot 回來：記憶體狀態中 show 已經是 true
  await mount(<Probe m={msg(EN)} />)
  assert.ok(document.querySelector('.msg-translation'), '重掛後仍是譯文')
  assert.equal(document.querySelector('.msg-swap-forward'), null, '重掛不可帶 msg-swap-forward')
  assert.equal(document.querySelector('.msg-swap-backward'), null, '重掛不可帶 msg-swap-backward')
  assert.equal(document.querySelector('.msg-swap-auto'), null, '重掛不可帶 msg-swap-auto')
  assert.equal(document.querySelector('.msg-translate-ico.spin-forward'), null, '重掛不可轉圖示')
  assert.equal(document.querySelector('.msg-translate-ico.spin-backward'), null, '重掛不可轉圖示')
})

test('prefers-reduced-motion：直接換面，正文與圖示都不播放動畫', async () => {
  const origMatchMedia = window.matchMedia
  try {
    window.matchMedia = (query: string) => ({
      matches: query.includes('prefers-reduced-motion: reduce'),
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList

    fakeApi((r) => (r.path.includes('/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
    await mount(<Probe m={msg(EN)} />)
    await click(document.querySelector('.msg-translate')!)
    await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
    assert.equal(document.querySelector('.msg-swap-forward'), null, 'reduced motion 不帶 msg-swap-forward')
    assert.equal(document.querySelector('.msg-translate-ico.spin-forward'), null, 'reduced motion 不轉圖示')
    assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 原文')
  } finally {
    window.matchMedia = origMatchMedia
  }
})



test('localStorage 有記錄時 mount 自動翻（載入中顯示原文＋翻譯中…），快取命中仍顯示原本金額、不寫未扣費', async () => {
  localStorage.setItem('am.msgTranslate.want', JSON.stringify(['m1']))
  resetTranslationStore()
  const reqs = fakeApi((r) => (r.path.endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: true, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translation') !== null, '自動翻出譯文')
  assert.equal(translateCalls(reqs), 1)
  assert.equal(document.querySelector('.msg-translation-cost')!.textContent, '(by Haiku $0.00042)')
  assert.doesNotMatch(document.querySelector('.msg-translation')!.textContent ?? '', /未扣費|快取/)
})

test('簡體轉繁（opencc-s2twp）標示 (by OpenCC)，不帶金額', async () => {
  localStorage.setItem('am.msgTranslate.want', JSON.stringify(['m1']))
  resetTranslationStore()
  fakeApi((r) => (r.path.endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'opencc-s2twp', cached: false, cost_usd: 0 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translation') !== null, '自動翻出譯文')
  assert.equal(document.querySelector('.msg-translation-cost')!.textContent, '(by OpenCC)')
})

test('自動翻失敗：不跳錯誤、同一頁面生命週期不重試，按鈕回「翻譯」', async () => {
  localStorage.setItem('am.msgTranslate.want', JSON.stringify(['m1']))
  resetTranslationStore()
  const reqs = fakeApi((r) => {
    if (r.path.endsWith('/claude-api/settings')) return SETTINGS_ON
    if (r.path.includes('/translate')) throw new Error('boom')
    return {}
  })
  await mount(<Probe m={msg(EN)} />)
  await until(() => translateCalls(reqs) === 1 && document.querySelector('.msg-translate')?.textContent === '翻譯', '失敗後回到「翻譯」')
  await unmountAll()
  await mount(<Probe m={msg(EN)} />)
  await settle(50)
  assert.equal(translateCalls(reqs), 1, '不再自動重試')
})

test('按回原文後 remount 仍是原文，且從記憶清單移除', async () => {
  const reqs = fakeApi(TR_ROUTE)
  await mount(<Probe m={msg(EN)} />)
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  assert.deepEqual(JSON.parse(localStorage.getItem('am.msgTranslate.want') ?? '[]'), ['m1'])
  await click(document.querySelector('.msg-translate')!)
  assert.deepEqual(JSON.parse(localStorage.getItem('am.msgTranslate.want') ?? '[]'), [])
  await unmountAll()
  await mount(<Probe m={msg(EN)} />)
  assert.equal(document.querySelector('.msg-translation'), null)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 譯文')
  assert.equal(translateCalls(reqs), 1)
})


