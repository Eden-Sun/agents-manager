import { spawn } from 'node:child_process'
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs'
const TOKEN = readFileSync(process.env.HOME + '/.config/agents-manager/ui-token', 'utf8').trim()
const URL_BASE = 'http://127.0.0.1:5173/?token=' + TOKEN
const OUT = process.env.OUT; mkdirSync(OUT, { recursive: true })
const PORT = 9381
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', `--user-data-dir=/tmp/am-mobile-profile`, '--window-size=390,844', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable')
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(500); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name, await ev('document.documentElement.scrollWidth + "x" + innerWidth')) }
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true })
await send('Page.navigate', { url: URL_BASE }); await sleep(3500)
await shot('m1-initial')
await ev(`document.querySelector('button[aria-label*="選單"], .menu-btn, button.hamburger')?.click()`); await sleep(800)
await shot('m2-drawer')
await ev(`[...document.querySelectorAll('.bot-row')].find(r=>/C1-fable|c1\\b/i.test(r.textContent))?.click()`); await sleep(1500)
await shot('m3-chat')
await ev(`[...document.querySelectorAll('.main-head button')].find(b=>b.textContent.trim()==='終端')?.click()`); await sleep(1500)
await shot('m4-terminal')
await ev(`[...document.querySelectorAll('.main-head button')].find(b=>b.textContent.trim()==='對話')?.click()`); await sleep(500)
const openDrawer = () => ev(`document.querySelector('button[aria-label*="選單"], .menu-btn, button.hamburger')?.click()`)
// team 節點在側欄裡，抽屜要先開。`.team-node-btn` 是它現在的 class；`開啟 Team` 是它 title
// 的開頭（`TeamNodes.tsx`），class 改名時還接得住。沒有進行中的 team 就整張跳過——
// 拍一張「其實是對話」的 m5-team 比沒有這張更誤導。
await openDrawer(); await sleep(600)
const teamHit = await ev(`(() => { const b = document.querySelector('.team-node-btn, [aria-label^="開啟 Team"], [title^="開啟 Team"]'); if (!b) return false; b.click(); return true })()`)
if (teamHit) { await sleep(1500); await shot('m5-team') }
else { console.log('m5-team: 側欄裡沒有 team 節點（目前沒有進行中的 team），跳過這張'); await ev(`document.querySelector('.scrim')?.click()`); await sleep(400) }
// 設定是 **bot** 標題列上的齒輪：剛剛選的是 team 的話那顆根本不存在，會拍成 team 畫面的
// 複本。所以先回到一顆 bot，再點齒輪。
await openDrawer(); await sleep(600)
await ev(`[...document.querySelectorAll('.bot-row')].find(r=>/C1-fable|c1\\b/i.test(r.textContent))?.click()`); await sleep(1500)
const gearHit = await ev(`(() => { const b = document.querySelector('.main-head .icon-btn.gear'); if (!b) return false; b.click(); return true })()`)
if (gearHit) { await sleep(1200); await shot('m6-settings') }
else console.log('m6-settings: 標題列上找不到設定齒輪，跳過這張')
chrome.kill(); process.exit(0)
