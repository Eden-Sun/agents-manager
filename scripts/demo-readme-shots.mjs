// README「畫面」那一節用的截圖，一次拍齊（深色）。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-readme-shots.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9353
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-readme', '--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
await send('Page.navigate', { url: URL_BASE }); await sleep(2400)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1440, height: 900, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const waitFor = async (sel, tries = 80) => { for (let i = 0; i < tries; i++) { if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return true; await sleep(250) } return false }
const startSelected = async () => { await ev(`[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()`); await sleep(2200) }
const typeSend = async (sel, text) => ev(`(()=>{const t=document.querySelector(${JSON.stringify(sel)});if(!t||t.disabled)return 'disabled';const s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(t,${JSON.stringify(text)});t.dispatchEvent(new Event('input',{bubbles:true}));t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'sent'})()`)
const pickBot = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith(${JSON.stringify(name)}));if(!r)return 'MISSING';r.click();return 'picked'})()`)

console.log('== 主畫面對話 ==')
console.log(' ', await pickBot('am-claude'))
await sleep(500)
await startSelected()
console.log(' ', await typeSend('.composer textarea', '幫我看一下 quota 那條在遠端主機上的行為'))
// 等回覆吐完（mock 會一路 streaming 到 assistant 氣泡）
await sleep(4500)
console.log('  訊息數:', await ev(`document.querySelectorAll('.msg').length`))
await shot('360-readme-chat-dark')

console.log('== 群組聊天 ==')
// 側欄那顆 ▶ 直接啟動，不用切到各自的對話面板（切回來會把剛剛的 run 選走）。
console.log('  啟動 am-codex:', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith('am-codex'));if(!r)return 'MISSING';const b=r.querySelector('.bot-run-btn.start');if(!b)return 'no start btn';b.click();return 'started'})()`))
await sleep(2600)
console.log('  燈號:', await ev(`[...document.querySelectorAll('.bot-row')].map(r=>(r.querySelector('.bot-name')?.textContent||'').trim().slice(0,9)+':'+([...r.querySelector('.lamp')?.classList??[]].find(c=>c.startsWith('lamp-'))??'?')).join(' | ')`))
await ev(`document.querySelector('.project-head')?.click()`)
await waitFor('.group-composer textarea')
await sleep(800)
console.log(' ', await typeSend('.group-composer textarea', '@all 這一版的額度條改成一次只看一台主機了，請各自確認'))
await sleep(4500)
console.log('  群組訊息:', await ev(`document.querySelectorAll('.msg-list.group .msg').length`))
await shot('361-readme-group-dark')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
