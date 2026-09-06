// blocked 自動彈出的全畫面 herdr 終端（mock: `VITE_MOCK=1 npx vite --port 5311`）。
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.AM_URL ?? 'http://localhost:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9351','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-blocked','--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9351/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const clickText = (t, sel='button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'missing '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const key = (k, code = k) => send('Input.dispatchKeyEvent',{type:'keyDown',key:k,code,windowsVirtualKeyCode:k==='Escape'?27:(k==='Enter'?13:(k.length===1?k.toUpperCase().charCodeAt(0):0))})
  .then(()=>k.length===1 ? send('Input.dispatchKeyEvent',{type:'char',text:k}) : null)
  .then(()=>send('Input.dispatchKeyEvent',{type:'keyUp',key:k,code,windowsVirtualKeyCode:k==='Escape'?27:(k==='Enter'?13:(k.length===1?k.toUpperCase().charCodeAt(0):0))}))
const state = async (tag) => console.log(
  tag,
  '| modal:', await ev(`!!document.querySelector('.blocked-modal')`),
  '| title:', await ev(`document.querySelector('.blocked-modal .blocked-title')?.textContent ?? '-'`),
  '| sub:', await ev(`document.querySelector('.blocked-modal .modal-sub')?.textContent?.trim().slice(0,60) ?? '-'`),
  '| term rows:', await ev(`(document.querySelector('.blocked-modal-term')?.textContent ?? '').split('\\n').length`),
  '| focus:', await ev(`document.activeElement?.className || document.activeElement?.tagName`),
  '| panel polling:', await ev(`document.querySelector('.blocked-sub')?.textContent?.includes('全畫面開著') ? 'paused' : 'live'`),
)

// 起手式：選一個跑著的 bot。
console.log('bots:', await ev(`[...document.querySelectorAll('.bot-row .bot-name')].map(e=>e.textContent.trim()).join(' | ')`))
console.log(await ev(`(()=>{const r=document.querySelector('.bot-row');if(!r)return 'no bot row';r.click();return 'selected '+r.textContent.trim().slice(0,20)})()`)); await sleep(800)
const botName = await ev(`document.querySelector('.main-title strong')?.textContent ?? '-'`)
console.log('selected bot:', botName)

// blocked 只有在 Run 跑著的時候才存在，所以先把 bot 啟動起來。
console.log(await clickText('啟動')); await sleep(4000)
console.log('run state:', await ev(`document.querySelector('.main-status')?.textContent?.trim() ?? '-'`), '| composer:', await ev(`!document.querySelector('.composer textarea')?.disabled`))

// agent 進 blocked → 全畫面自己跳出來。
console.log(await ev(`window.__amMock.block(${JSON.stringify(botName)}) ?? 'blocked'`)); await sleep(1800)
await state('auto-open   ')
await shot('340-blocked-modal')

// 鍵盤直通（單字元）：焦點在終端上直接按 n，mock 收到就把 agent 放回 idle → 視窗自己收起來。
// 這同時證明按鍵有送出去，以及「離開 blocked 就關窗」。
await key('n'); await sleep(1200)
await state('after n     ')
console.log('agent status:', await ev(`document.querySelector('.main-status')?.textContent?.trim() ?? '-'`))

// 再 blocked 一次：上一輪已經離開過 blocked，所以應該重新自動彈出。
console.log(await ev(`window.__amMock.block(${JSON.stringify(botName)}) ?? 'blocked'`)); await sleep(1800)
await state('second time ')

// ✕ 關掉 → 底下的面板接手，並且不會自己彈回來。
console.log(await ev(`document.querySelector('.blocked-modal .icon-btn').click() ?? 'closed via ✕'`)); await sleep(1500)
await state('after close ')
console.log('panel still there:', await ev(`!!document.querySelector('.blocked')`), '| expand button:', await ev(`!!document.querySelector('.blocked-head .mini-btn')`))
await shot('341-blocked-panel-after-close')

// 手動展開全畫面。
console.log(await clickText('展開全畫面')); await sleep(700)
await state('re-expanded ')

// 按鍵列的 Enter（滑鼠）→ mock 讓 agent 回到 working，全畫面自己收起來。
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.blocked-modal .key-btn')].find(x=>x.textContent.trim()==='Enter');if(!b)return 'no Enter key';b.click();return 'clicked keypad Enter'})()`)); await sleep(1200)
await state('after Enter ')

// 深色再看一次。
await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'dark'}]})
await sleep(2500)
console.log(await ev(`window.__amMock.block(${JSON.stringify(botName)}) ?? 'blocked again'`)); await sleep(1800)
await state('dark        ')
await shot('342-blocked-modal-dark')

console.log('--- errors ---')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
