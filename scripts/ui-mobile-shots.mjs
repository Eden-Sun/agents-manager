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
await ev(`document.querySelector('button[aria-label*="選單"], .menu-btn, button.hamburger')?.click()`); await sleep(600)
// 沒有進行中的 team 時這一下點不到任何東西，抽屜也不會自己收（選取沒變）——那就把它關掉，
// 免得 m5 拍出來只是 m2 的複本。印一行說明，看圖的人才知道這張為什麼是對話而不是 team。
const teamHit = await ev(`(() => { const b = document.querySelector('.team-node-btn'); if (!b) return false; b.click(); return true })()`)
if (!teamHit) { console.log('m5: 目前沒有進行中的 team，改拍收起抽屜後的畫面'); await ev(`document.querySelector('.scrim')?.click()`) }
await sleep(1500)
await shot('m5-team')
// 設定是從標題列的齒輪開的（側欄那顆藏在 `⋯` 選單裡，要先展開才點得到）。
await ev(`document.querySelector('.main-head .icon-btn.gear')?.click()`); await sleep(1200)
await shot('m6-settings')
chrome.kill(); process.exit(0)
