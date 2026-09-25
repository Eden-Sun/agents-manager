// 「需要回應」框在手機上點得到選項（issue #559）：量版面、真的點一輪，量不過 exit 1。
//
// 2026-09-25 使用者手機截圖：兩題 AskUserQuestion、終端太矮把分頁列與第二題的題目裁掉，從對話紀錄補回來的原題卡把整個框吃掉，
// 能按的選項只露出半列。單看截圖分不出「在捲軸外」還是「被底部按鈕蓋住」，所以量數字：
//
//   1. 第一個可點選項整列落在可視範圍內（視窗、`.blocked-modal-body` 的捲軸都要），而且點它的中心點到的就是它
//      （沒被釘住的題目或底部按鈕列蓋住）——390×844、390×640（iOS 瀏覽器扣掉網址列與工具列）、1440×900 三種。
//   2. 手機上一題一題答完：第一題點選項 → 第二題（可複選）勾一項 → 送出（Submit）→ review 頁 Submit answers，
//      每一步要點的東西都在第一屏、點得到，最後框自己關掉（mock 收到交卷）。
//
// 走 mock（`VITE_MOCK=1`，`__amMock.twoAsk()`），不碰正式 daemon 也不碰 5173；vite 與 Chrome 都自己起、用完就收，
// 暫存 profile 用完就刪。
//   node scripts/verify-blocked-mobile.mjs
//   OUT=docs/screenshots/blocked-mobile SHOT_PREFIX=after node scripts/verify-blocked-mobile.mjs   # 順便存圖
import { spawn } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const WEB = join(dirname(fileURLToPath(import.meta.url)), '..', 'web')
const VITE_PORT = Number(process.env.VITE_PORT ?? 5213)
const BASE = process.env.BASE ?? `http://127.0.0.1:${VITE_PORT}/`
const CDP_PORT = Number(process.env.CDP_PORT ?? 9431)
const OUT = process.env.OUT ?? null
const PREFIX = process.env.SHOT_PREFIX ?? 'shot'
if (OUT) mkdirSync(OUT, { recursive: true })

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const failures = []
const check = (ok, what) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`)
  if (!ok) failures.push(what)
}

const vite = process.env.BASE
  ? null
  : spawn('bunx', ['vite', '--port', String(VITE_PORT), '--strictPort'], {
      cwd: WEB,
      env: { ...process.env, VITE_MOCK: '1' },
      stdio: 'ignore',
    })
const profile = mkdtempSync(join(tmpdir(), 'am-blocked-mobile-'))
let chrome = null

try {
  for (let i = 0; i < 120; i += 1) {
    try {
      if ((await fetch(BASE)).ok) break
    } catch {}
    await sleep(250)
  }
  chrome = spawn(
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    ['--headless=new', `--remote-debugging-port=${CDP_PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run',
      `--user-data-dir=${profile}`, '--window-size=1440,900', 'about:blank'],
    { stdio: 'ignore' },
  )
  let ws
  for (let i = 0; i < 100 && !ws; i += 1) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json()
      const page = pages.find((t) => t.type === 'page')
      if (page) ws = new WebSocket(page.webSocketDebuggerUrl)
    } catch {}
    if (!ws) await sleep(250)
  }
  if (!ws) throw new Error(`Chrome did not expose CDP on ${CDP_PORT}`)
  let id = 0
  const pending = new Map()
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data)
    if (m.id && pending.has(m.id)) {
      const p = pending.get(m.id)
      pending.delete(m.id)
      m.error ? p.reject(new Error(JSON.stringify(m.error))) : p.resolve(m.result)
    }
  }
  await new Promise((r) => (ws.onopen = r))
  const send = (method, params = {}) => {
    const i = ++id
    ws.send(JSON.stringify({ id: i, method, params }))
    return new Promise((resolve, reject) => pending.set(i, { resolve, reject }))
  }
  const ev = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed')
    return r.result?.value
  }
  const waitFor = async (expression, ms = 8000) => {
    for (let t = 0; t < ms; t += 150) {
      if (await ev(expression)) return true
      await sleep(150)
    }
    return false
  }
  const shot = async (name) => {
    if (!OUT) return
    await sleep(300)
    const { data } = await send('Page.captureScreenshot', { format: 'png' })
    writeFileSync(`${OUT}/${PREFIX}-${name}.png`, Buffer.from(data, 'base64'))
    console.log('saved', `${PREFIX}-${name}`)
  }
  await send('Runtime.enable')
  await send('Page.enable')

  /**
   * 這個元素整個在視窗裡、在每一層會裁切的祖先裡，而且點它的中心點到的是它自己（沒被別的東西蓋住）。
   * 回 `{ ok, why, rect }`；`sel` 是 CSS selector，`text` 有給就挑內容含這段字的那一個。
   */
  const reach = (sel, text = null) =>
    ev(`(() => {
      const els = [...document.querySelectorAll(${JSON.stringify(sel)})]
      const el = ${text === null ? 'els[0]' : `els.find((e) => e.textContent.includes(${JSON.stringify(text)}))`}
      if (!el) return { ok: false, why: 'not rendered' }
      const r = el.getBoundingClientRect()
      const rect = { top: Math.round(r.top), bottom: Math.round(r.bottom), left: Math.round(r.left), right: Math.round(r.right) }
      if (r.height < 20) return { ok: false, why: 'too short (' + r.height + 'px)', rect }
      if (r.top < 0 || r.bottom > innerHeight + 0.5 || r.left < 0 || r.right > innerWidth + 0.5) return { ok: false, why: 'outside viewport ' + innerWidth + 'x' + innerHeight, rect }
      for (let p = el.parentElement; p && p !== document.body; p = p.parentElement) {
        const cs = getComputedStyle(p)
        if (cs.overflowY === 'visible' && cs.overflowX === 'visible') continue
        const c = p.getBoundingClientRect()
        if (r.top < c.top - 0.5 || r.bottom > c.bottom + 0.5) return { ok: false, why: 'clipped by .' + String(p.className).split(' ').join('.') + ' [' + Math.round(c.top) + ',' + Math.round(c.bottom) + ']', rect }
      }
      const x = (r.left + r.right) / 2, y = (r.top + r.bottom) / 2
      const hit = document.elementFromPoint(x, y)
      if (!hit || !(el === hit || el.contains(hit))) return { ok: false, why: 'covered by ' + (hit ? hit.tagName + '.' + String(hit.className) : 'nothing'), rect }
      return { ok: true, rect, x, y }
    })()`)

  const tap = async (x, y) => {
    for (const type of ['mousePressed', 'mouseReleased']) {
      await send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 })
    }
  }

  /** 開一顆新的 mock、進兩題情境、選到那顆 bot，等全畫面框與原題都出來。 */
  const openAsk = async (w, h, mobile) => {
    await send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 2, mobile })
    await send('Page.navigate', { url: BASE })
    await waitFor(`typeof globalThis.__amMock?.twoAsk === 'function' && document.querySelector('.sidebar, .main-head') !== null`, 15000)
    await sleep(1200)
    const botId = await ev(`globalThis.__amMock.twoAsk()`)
    if (!botId) throw new Error('mock 沒有在跑的 claude bot')
    await ev(`history.pushState(null, '', '/bots/' + ${JSON.stringify(botId)}); dispatchEvent(new PopStateEvent('popstate'))`)
    const opened = await waitFor(`document.querySelector('.blocked-modal .bc-item') !== null && document.querySelector('.blocked-modal .pending-q') !== null`, 15000)
    check(opened, `${w}x${h}: 需要回應框彈出、認出選單、補了原題`)
    await sleep(400)
    return opened
  }

  const firstOption = async (label) => {
    const r = await reach('.blocked-modal .bc-item')
    check(r.ok, `${label}: 第一個可點選項在第一屏、點得到${r.ok ? ` (top ${r.rect.top}, bottom ${r.rect.bottom})` : ` — ${r.why} ${JSON.stringify(r.rect ?? {})}`}`)
    return r
  }

  // 1. 三種尺寸量第一個選項。
  for (const [w, h, mobile, name] of [[390, 844, true, 'phone-390x844'], [390, 640, true, 'phone-390x640'], [1440, 900, false, 'desktop-1440x900']]) {
    if (!(await openAsk(w, h, mobile))) continue
    await firstOption(name)
    const q = await ev(`document.querySelector('.blocked-modal .bc-question')?.textContent ?? ''`)
    check(q.includes('要關票嗎'), `${name}: 選項上面看得到這一題的題目`)
    await shot(name)
    // 原題預設收成一行；展開之後排在選項下面，第一個選項不能被推走。
    const collapsed = await ev(`document.querySelector('.blocked-modal .pending-q-toggle')?.getAttribute('aria-expanded') === 'false' && !document.querySelector('.blocked-modal .pending-q-item')`)
    check(collapsed, `${name}: 原題預設收成一行`)
    await ev(`document.querySelector('.blocked-modal .pending-q-toggle')?.click()`)
    await sleep(300)
    check(await ev(`document.querySelectorAll('.blocked-modal .pending-q-item').length === 2`), `${name}: 原題展開後兩題都在`)
    await firstOption(`${name} 展開原題後`)
    await shot(`${name}-expanded`)
  }

  // 2. 手機一題一題答完（390×640，最擠的那個）。
  if (await openAsk(390, 640, true)) {
    const step = async (label, sel, text) => {
      // 上一步的按鍵還在送（選單整份 disabled）時點下去會被吃掉：等它能按。
      const pick = text === null || text === undefined ? 'els[0]' : `els.find((e) => e.textContent.includes(${JSON.stringify(text)}))`
      await waitFor(`(() => { const els = [...document.querySelectorAll(${JSON.stringify(sel)})]; const el = ${pick}; return Boolean(el && !el.disabled) })()`)
      const r = await reach(sel, text)
      check(r.ok, `作答：${label} 在第一屏、點得到${r.ok ? '' : ` — ${r.why} ${JSON.stringify(r.rect ?? {})}`}`)
      if (r.ok) await tap(r.x, r.y)
      return r.ok
    }
    if (await step('第一題「關票」', '.blocked-modal .bc-item', '關票')) {
      const q2 = await waitFor(`[...document.querySelectorAll('.blocked-modal .bc-item')].some((b) => b.textContent.includes('改啟動鍵文字'))`)
      check(q2, '作答：第一題選完跳到第二題（可複選）')
      await sleep(400)
      const q2text = await ev(`document.querySelector('.blocked-modal .bc-question')?.textContent ?? ''`)
      check(q2text.includes('兩個小取捨'), '作答：第二題畫面沒畫出題目，選項上面照樣有題目（從原題補）')
      await firstOption('作答：第二題')
      await shot('flow-q2')
      if (q2 && (await step('第二題勾「改啟動鍵文字」', '.blocked-modal .bc-item', '改啟動鍵文字'))) {
        const ticked = await waitFor(`[...document.querySelectorAll('.blocked-modal .bc-item')].find((b) => b.textContent.includes('改啟動鍵文字'))?.getAttribute('aria-pressed') === 'true'`)
        check(ticked, '作答：複選那一項勾起來了')
        if (ticked && (await step('送出（Submit）', '.blocked-modal .bc-submit'))) {
          const review = await waitFor(`[...document.querySelectorAll('.blocked-modal .bc-item')].some((b) => b.textContent.includes('Submit answers'))`)
          check(review, '作答：進 review 頁')
          await shot('flow-review')
          if (review && (await step('Submit answers', '.blocked-modal .bc-item', 'Submit answers'))) {
            check(await waitFor(`document.querySelector('.blocked-modal') === null`), '作答：交卷後框自己關掉')
          }
        }
      }
    }
  }
} catch (e) {
  failures.push(String(e?.stack ?? e))
  console.log('ERROR', e)
} finally {
  chrome?.kill()
  vite?.kill()
  await sleep(300)
  rmSync(profile, { recursive: true, force: true })
}

console.log(failures.length ? `\n${failures.length} 項沒過` : '\n全部通過')
process.exit(failures.length ? 1 : 0)
