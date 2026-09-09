// 每個畫面都有自己的 URL：對著 5173（真 daemon）走一遍路由，順便留下截圖。
// `BOT=<id> TEAM=<id> OUT=dir node scripts/ui-routes-shots.mjs`
import { spawn } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
const TOKEN = process.env.AM_TOKEN ?? readFileSync(process.env.HOME + '/.config/agents-manager/ui-token', 'utf8').trim()
const BOT = process.env.BOT
const TEAM = process.env.TEAM
const PROJECT = process.env.PROJECT
const BASE = process.env.BASE ?? 'http://127.0.0.1:5173'
const OUT = process.env.OUT ?? '/tmp/am-ui-routes'
if (!BOT) { console.error('need BOT=<bot id>'); process.exit(2) }
mkdirSync(OUT, { recursive: true })
const PORT = 9391
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-ui-routes-profile', '--window-size=1440,900', 'about:blank'], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page'); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable')
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); }
const url = () => ev('location.pathname + location.search')
const title = () => ev('document.title')
const goto = async (path, wait = 3500) => { await send('Page.navigate', { url: BASE + path }); await sleep(wait) }
const fails = []
const check = async (label, got, want) => { const ok = got === want; console.log(`${ok ? 'PASS' : 'FAIL'}  ${label}: ${JSON.stringify(got)}${ok ? '' : ' want ' + JSON.stringify(want)}`); if (!ok) fails.push(label) }

await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })

// 1. 直接開 /bots/<id>?token=…：落在那個 bot，token 從網址上消失。
await goto(`/bots/${BOT}?token=${TOKEN}`)
await check('open /bots/<id> 停在同一個路徑', await url(), `/bots/${BOT}`)
await check('選到的就是那個 bot', await ev(`document.querySelector('.bot-row.selected')?.dataset.botId`), BOT)
console.log('      title:', await title())
await shot('r1-bot-direct')

// 2. 對話 → 終端：同一個畫面，用 replaceState（上一頁不該多一格）。
const depth0 = await ev('history.length')
await ev(`[...document.querySelectorAll('.main-head button')].find(b=>b.textContent.trim()==='終端')?.click()`); await sleep(1200)
await check('點終端 → /terminal', await url(), `/bots/${BOT}/terminal`)
await check('對話↔終端不多一格歷史', await ev('history.length'), depth0)
await shot('r2-terminal')

// 3. 換一個 bot（push）再上一頁：回到終端那個畫面。
await goto(`/bots/${BOT}/terminal`, 3500)
await ev(`[...document.querySelectorAll('.bot-row')].filter(r=>r.dataset.botId!=='${BOT}')[0]?.click()`); await sleep(1200)
const other = await ev(`document.querySelector('.bot-row.selected')?.dataset.botId`)
await check('換 bot → /bots/<other>', await url(), `/bots/${other}`)
await ev('history.back()'); await sleep(1500)
await check('上一頁回到終端分頁', await url(), `/bots/${BOT}/terminal`)
await check('分頁真的切回終端', await ev(`document.querySelector('[role=tab][aria-selected=true]')?.textContent.trim()`), '終端')
await shot('r3-back-to-terminal')

// 4. 設定浮窗：開著 push、關掉是上一頁。
await goto(`/bots/${BOT}`)
await ev(`document.querySelector('.main-head .icon-btn.gear')?.click()`); await sleep(1200)
await check('開設定 → /settings', await url(), `/bots/${BOT}/settings`)
await check('設定浮窗開著', await ev(`!!document.querySelector('.bot-settings')`), true)
await shot('r4-settings')
await ev('history.back()'); await sleep(1200)
await check('上一頁關掉設定', await url(), `/bots/${BOT}`)
await check('設定浮窗收起來了', await ev(`!document.querySelector('.bot-settings')`), true)

// 5. Team：直接開、重新整理停在同一個畫面。
if (TEAM) {
  await goto(`/teams/${TEAM}`)
  await check('open /teams/<id>', await url(), `/teams/${TEAM}`)
  await check('畫面是 TeamPanel', await ev(`!!document.querySelector('.team-panel, .team-head')`), true)
  console.log('      title:', await title())
  await shot('r5-team')
  await send('Page.reload'); await sleep(3500)
  await check('reload 停在同一個 team', await url(), `/teams/${TEAM}`)
  await shot('r6-team-reload')
}

// 5b. Project 群組聊天與組隊 sheet（`?issue=` 要留著）。
if (PROJECT) {
  await goto(`/projects/${PROJECT}`)
  await check('open /projects/<id>', await url(), `/projects/${PROJECT}`)
  await check('畫面是群組聊天', await ev(`!!document.querySelector('.group-tag')`), true)
  await shot('r5b-project')
  await goto(`/projects/${PROJECT}/teams/new?issue=48`)
  await check('open 組隊 sheet 且保留 issue', await url(), `/projects/${PROJECT}/teams/new?issue=48`)
  await check('畫面是 TeamLaunchPanel', await ev(`!!document.querySelector('.team-launch')`), true)
  await shot('r5c-team-new')
}

// 6. 壞連結：回首頁並說一聲。
await goto('/bots/does-not-exist', 2200)
await check('不存在的 bot 回首頁', (await url()).startsWith('/bots/does-not-exist'), false)
await shot('r7-missing-bot')
await check('有跳通知', await ev(`[...document.querySelectorAll('.notice')].some(n=>n.textContent.includes('已回到首頁'))`), true)

// 7. 手機抽屜：開著時按上一頁 = 關抽屜，不換畫面。
await goto(`/bots/${BOT}`)
await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true }); await sleep(900)
await ev(`[...document.querySelectorAll('button')].find(b=>b.getAttribute('aria-label')?.includes('側邊欄')||b.textContent.trim()==='☰')?.click()`); await sleep(700)
await check('抽屜開著', await ev(`!!document.querySelector('.sidebar.open')`), true)
await shot('r8-drawer-open')
await ev('history.back()'); await sleep(900)
await check('上一頁關抽屜', await ev(`!!document.querySelector('.sidebar.open')`), false)
await check('畫面沒被換掉', await url(), `/bots/${BOT}`)
await shot('r9-drawer-back')

console.log(fails.length ? `\n${fails.length} FAILED: ${fails.join(', ')}` : `\nall checks passed; shots in ${OUT}`)
chrome.kill(); process.exit(fails.length ? 1 : 0)
