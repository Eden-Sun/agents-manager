import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:7788/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
mkdirSync(OUT, { recursive: true })
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9336','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-demo','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9336/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const clickText = (t, sel='button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'missing '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const selectBot = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.textContent.includes(${JSON.stringify(name)}));if(!r)return 'no bot';r.click();return 'selected ${name}'})()`)
const lamps = () => ev(`[...document.querySelectorAll('.bot-row')].map(r=>r.querySelector('.bot-name')?.textContent+':'+[...r.querySelectorAll('.lamp')].map(l=>l.className.replace('lamp','').trim()).join('')).join(' | ')`)
const msgCount = () => ev(`document.querySelectorAll('.msg').length`)
const lastMsg = () => ev(`(()=>{const m=[...document.querySelectorAll('.msg')].pop();return m?m.className+' :: '+m.textContent.slice(0,200):'none'})()`)

console.log('booted:', await ev(`!!document.querySelector('.app')`), '| lamps:', await lamps())
await shot('30-demo-overview')
console.log(await selectBot('am-claude')); await sleep(1200)
const before = await msgCount(); console.log('msgs before:', before)
const marker = 'DEMO-' + Date.now().toString(36).toUpperCase()
console.log('typing prompt with marker', marker)
await ev(`(()=>{const t=document.querySelector('.composer textarea');const s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(t,${JSON.stringify('Reply with exactly the single token '+marker+' and nothing else.')});t.dispatchEvent(new Event('input',{bubbles:true}));return 'typed'})()`)
await sleep(300); await shot('31-demo-typed')
console.log(await ev(`(()=>{const t=document.querySelector('.composer textarea');t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'enter'})()`))
await sleep(1500); console.log('after send lamps:', await lamps(), '| msgs:', await msgCount()); await shot('32-demo-working')
let reply = null
for (let i=0;i<40;i++){ await sleep(1000); const l = await lastMsg(); if (l.includes(marker) && l.includes('assistant')) { reply = l; break } }
console.log('reply:', reply ?? 'TIMEOUT last=' + await lastMsg())
console.log('lamps after reply:', await lamps())
await shot('33-demo-reply-hook')
console.log(await clickText('終端', '.tab')); await sleep(1800)
console.log('terminal contains marker:', String(await ev(`document.querySelector('.term')?.textContent`)).includes(marker))
await shot('34-demo-terminal')
console.log(await clickText('對話', '.tab')); await sleep(400)
// stop codex bot then start it again
console.log(await selectBot('am-codex')); await sleep(800)
console.log(await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.textContent.includes('am-codex'));const b=[...r.querySelectorAll('button')].find(b=>/stop|停止/i.test(b.textContent));if(!b)return 'no stop btn: '+[...r.querySelectorAll('button')].map(b=>b.textContent).join(',');b.click();return 'clicked stop'})()`))
for (let i=0;i<15;i++){ await sleep(1000); const l = await lamps(); if (/am-codex:[^|]*(offline|stopped)/.test(l)) break }
console.log('after stop lamps:', await lamps()); await shot('35-demo-stopped')
console.log(await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.textContent.includes('am-codex'));const b=[...r.querySelectorAll('button')].find(b=>/start|啟動/i.test(b.textContent));if(!b)return 'no start btn';b.click();return 'clicked start'})()`))
await sleep(1500); console.log('starting lamps:', await lamps()); await shot('36-demo-starting')
for (let i=0;i<30;i++){ await sleep(1000); const l = await lamps(); if (/am-codex:[^|]*idle/.test(l)) break }
console.log('after start lamps:', await lamps()); await shot('37-demo-restarted')
console.log('--- page errors ---')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
for (const e of events) if (e.method==='Runtime.consoleAPICalled' && e.params.type==='error') console.log('console.error:', JSON.stringify(e.params.args).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
