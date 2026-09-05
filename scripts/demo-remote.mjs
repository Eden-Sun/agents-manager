import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:7788/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9342','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-remote2','--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9342/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
console.log('host badges:', await ev(`[...document.querySelectorAll('.project-head .host-badge')].map(e=>e.textContent+':'+e.className).join(', ')`))
console.log('identity badges:', await ev(`[...document.querySelectorAll('.bot-row .identity-badge')].map(e=>e.textContent).join(', ')`))
console.log(await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.bot-name')?.textContent==='pt-claude');if(!r)return 'no remote bot';r.click();return 'selected remote bot'})()`)); await sleep(1500)
const marker = 'REMOTE-' + Date.now().toString(36).toUpperCase()
await ev(`(()=>{const t=document.querySelector('.composer textarea');if(!t||t.disabled)return 'composer disabled: '+(document.querySelector('.composer-lock')?.textContent);const s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(t,${JSON.stringify('Reply with exactly the single token '+marker+' and nothing else.')});t.dispatchEvent(new Event('input',{bubbles:true}));t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'sent'})()`).then(r=>console.log(r))
await sleep(1500); console.log('after send lamp:', await ev(`[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.bot-name')?.textContent==='pt-claude')?.querySelector('.lamp')?.className`))
let reply=null
for (let i=0;i<60;i++){ await sleep(1000); const l = await ev(`(()=>{const m=[...document.querySelectorAll('.msg')].pop();return m?m.className+' :: '+m.textContent.slice(0,160):'none'})()`); if (l.includes(marker) && l.includes('assistant')) { reply=l; break } }
console.log('remote reply:', reply ?? 'TIMEOUT')
await ev(`document.querySelector('.msg-list').scrollTop = 1e9`)
await shot('90-remote-bot-reply')
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.disclosure')].find(b=>b.textContent.includes('主機'));b.click();return 'opened hosts'})()`)); await sleep(500)
console.log('host rows:', await ev(`[...document.querySelectorAll('.host-row')].map(r=>r.querySelector('.host-name')?.textContent+' '+r.querySelector('.lamp')?.className).join(' | ')`))
await shot('91-hosts-panel-m4p')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
