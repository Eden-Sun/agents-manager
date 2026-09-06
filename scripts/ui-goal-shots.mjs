// Seven screenshots of the live dev UI (5173, real daemon) for the UI polish goal:
// chat dark/light, team, terminal, bot settings, 1024px, mobile. `OUT=dir node scripts/ui-goal-shots.mjs`.
import { spawn } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
const TOKEN = process.env.AM_TOKEN ?? readFileSync(process.env.HOME + '/.config/agents-manager/ui-token', 'utf8').trim()
const URL_BASE = 'http://127.0.0.1:5173/?token=' + TOKEN
const OUT = process.env.OUT ?? '/tmp/am-ui-goal'
mkdirSync(OUT, { recursive: true })
const PORT = 9378
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', `--user-data-dir=/tmp/am-ui-goal-profile`, '--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable')
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
const dark = (on) => send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: on ? 'dark' : 'light' }] })
await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
await dark(true); await send('Page.navigate', { url: URL_BASE }); await sleep(3500)
await ev(`[...document.querySelectorAll('.bot-row')].find(r=>r.textContent.includes('C1-Fable'))?.click()`); await sleep(1500)
await shot('s1-chat-dark')
await dark(false); await sleep(300); await shot('s2-chat-light'); await dark(true)
await ev(`[...document.querySelectorAll('.team-node-btn')][0]?.click()`); await sleep(1500)
await shot('s3-team-dark')
await ev(`[...document.querySelectorAll('.bot-row')].find(r=>r.textContent.includes('C1-Fable'))?.click()`); await sleep(800)
await ev(`[...document.querySelectorAll('.main-head button')].find(b=>b.textContent.trim()==='終端')?.click()`); await sleep(1500)
await shot('s4-terminal-dark')
await ev(`[...document.querySelectorAll('.main-head button')].find(b=>b.textContent.trim()==='對話')?.click()`); await sleep(500)
await ev(`[...document.querySelectorAll('.bot-row')].find(r=>r.textContent.includes('C1-Fable'))?.querySelector('button[aria-label^="設定"]')?.click()`); await sleep(1200)
await shot('s5-botsettings-dark')
await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape',windowsVirtualKeyCode:27}); await send('Input.dispatchKeyEvent',{type:'keyUp',key:'Escape',code:'Escape',windowsVirtualKeyCode:27}); await sleep(400)
await send('Emulation.setDeviceMetricsOverride', { width: 1024, height: 800, deviceScaleFactor: 1, mobile: false }); await sleep(800)
await shot('s6-1024-dark')
await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true }); await sleep(800)
await shot('s7-mobile-dark')
chrome.kill(); process.exit(0)
