// 未讀的另外兩件事，用真後端（`agents-managerd serve` + `npx vite` @ 5173）驗：
//   1. 跨重新整理保留（帳本在 localStorage，bot id 是真的、重整後還在）
//   2. 「在看」的條件是分頁可見 ＋ 視窗有 focus——只是選到它不算
// 只讀畫面、只寫 localStorage，不對任何 bot 送訊息。
// Usage: node scripts/demo-unread-persist.mjs [http://127.0.0.1:5173/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5173/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots/unread'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9358
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-unread2', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name, h = 900) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1280, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const badges = () => ev(`JSON.stringify([...document.querySelectorAll('.bot-row')].map(r=>({id:r.dataset.botId,badge:r.querySelector('.unread-turns')?.textContent??null})).filter(x=>x.badge))`)

// 要挑一個**沒有被選取**的 bot：正在看的那個一載入就會被標成已讀（那正是它該做的事）。
const botId = await ev(`[...document.querySelectorAll('.bot-row')].find(r=>!r.classList.contains('selected'))?.dataset.botId ?? ''`)
console.log('  拿一個沒被選取的真 bot id:', botId)
if (!botId) { console.log('沒有 bot，收工'); ws.close(); chrome.kill(); process.exit(1) }

console.log('== 帳本寫進 localStorage，重新整理 ==')
await ev(`localStorage.setItem('am.unread', JSON.stringify({'bot:${botId}':3}));localStorage.setItem('am.readMarks',JSON.stringify({'bot:${botId}':{at:'2020-01-01T00:00:00Z',id:''}}));'seeded'`)
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
console.log('  徽章:', await badges())
console.log('  標題:', await ev('document.title'))
await shot('444-unread-persisted')

console.log('== 分頁在背景時點進去：不算讀到（徽章留著） ==')
console.log(' ', await ev(`(()=>{Object.defineProperty(document,'visibilityState',{value:'hidden',configurable:true});document.hasFocus=()=>false;document.dispatchEvent(new Event('visibilitychange'));return document.visibilityState})()`))
await ev(`document.querySelector('.bot-row[data-bot-id="${botId}"]')?.click()`)
await sleep(800)
// 點下去會載入訊息，`recountBot` 就用上面那個假的已讀標記（2020 年）重算——真實對話裡
// 2020 年之後的回合全部算未讀，所以數字會從 3 跳成「這個 bot 的歷史回合數」。那正是
// 「開機用 messages 比對算出未讀數」這條路被走到的證據。
console.log('  徽章:', await badges(), ' 標題:', await ev('document.title'))
await shot('445-unread-hidden-tab')

console.log('== 回到前景（visibilitychange）：現在開著的那個對話就清掉 ==')
console.log(' ', await ev(`(()=>{Object.defineProperty(document,'visibilityState',{value:'visible',configurable:true});document.hasFocus=()=>true;document.dispatchEvent(new Event('visibilitychange'));return document.visibilityState})()`))
await sleep(800)
console.log('  徽章:', await badges(), ' 標題:', await ev('document.title'))
console.log('  帳本:', await ev(`localStorage.getItem('am.unread')`))
await shot('446-unread-cleared-on-focus')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
