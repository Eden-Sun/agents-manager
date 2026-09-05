import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:7788/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9354','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-real-shots','--window-size=1440,900', 'about:blank'], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9354/json/list')).json(); const p = l.find(t=>t.type==='page'); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(6000)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC' : r.result?.value }
const shot = async (name, dark) => { await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:dark?'dark':'light'}]}); await sleep(500); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.bot-name')?.textContent==='am-claude');r?.click()})()`); await sleep(1200)
await ev(`document.querySelector('.msg-list').scrollTop = 1e9`)
await shot('170-real-chat-dark', true)
await shot('171-real-chat-light', false)
await ev(`(()=>{const b=[...document.querySelectorAll('.project-head button')].find(b=>b.classList.contains('project-label-btn'));b?.click()})()`); await sleep(1500)
await ev(`(()=>{const l=document.querySelector('.msg-list, [class*=group] [class*=list]');if(l)l.scrollTop=1e9})()`)
await shot('172-real-group-dark', true)
await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.bot-name')?.textContent==='am-codex');r?.click()})()`); await sleep(800)
await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>/設定/.test(b.title||'')||/⚙/.test(b.textContent));b?.click()})()`); await sleep(600)
await shot('173-real-settings-dark', true)
await ev(`document.querySelector('.project-head .icon-btn.add')?.click()`); await sleep(500)
await shot('174-real-newbot-dark', true)
ws.close(); chrome.kill(); process.exit(0)
