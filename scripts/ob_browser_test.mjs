import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const script = readFileSync(new URL('./chatgpt-consult.mjs', import.meta.url), 'utf8');
const pid = '01M1Y7BNVP843V9MFEDJ2KW9NQ';
const key = '1234567890abcdef1234567890abcdef';
const url = 'https://chatgpt.com/c/project-a';
const marker = `[OB request=${key}]`;
// Rows are [role, text, completed?]; completed turns carry copy-turn-action-button.
const boundTurn = [['user', marker + ' question'], ['assistant', 'BOUND_ANSWER', true],
  ['user', 'unrelated later question'], ['assistant', 'WRONG_LATER_ANSWER', true]];
const olderHistory = n => Array.from({length: n}, (_, i) => [['user', `old ${i}`], ['assistant', `old answer ${i}`, true]]).flat();

// The mock DOM is stateful and waitForFunction really polls the page condition against it:
// `rows` is the initial conversation, `afterSend` is appended when send is clicked and
// `events` ([{at, rows?, stop?}]) change the DOM at a given poll to simulate streaming.
function run(options = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'ob-browser-test-'));
  try {
    const journal = join(dir, 'journal.json');
    if (options.progress) writeFileSync(journal, JSON.stringify(options.progress));
    // rotate = 這個 project 已經有一串（`previous_url`），但這一題要開新的一串（`url` 空）。
    const args = {project_id: pid, project_label: 'AM', question: 'review this', request_key: key,
      url: options.newProject || options.rotate ? null : url,
      previous_url: options.rotate ? url : null,
      journal, collect: !!options.collect, timeout_ms: 600000};
    const prelude = `
      const actions=[];
      const opts=${JSON.stringify(options)};
      const base=${JSON.stringify(url)};
      let currentUrl=opts.newProject?'https://chatgpt.com/':base;
      let rows=opts.rows||[];
      let stop=!!opts.stop;
      let polls=0;
      const node=([role,text,done])=>({innerText:text,
        getAttribute:a=>a==='data-message-author-role'?role:null,
        closest:s=>s.includes('data-turn="'+role+'"')?{querySelector:q=>done&&q.includes('copy-turn-action-button')?{}:null}:null});
      globalThis.location={get pathname(){return new URL(currentUrl).pathname;}};
      globalThis.document={
        querySelectorAll:s=>rows.map(node).filter(n=>!s.includes('="assistant"')||n.getAttribute('data-message-author-role')==='assistant'),
        querySelector:s=>s.includes('stop-button')&&stop?{}:null};
      const page={label:'p2',goto:async u=>{actions.push(['goto',u]);currentUrl=u;},
        waitForSelector:async()=>{},snapshot:async()=>'',
        click:async s=>{actions.push(['click',s]);
          if(!s.includes('send-button'))return;
          if(opts.failSend)throw new Error('send interrupted');
          rows=rows.concat(opts.afterSend||${JSON.stringify(boundTurn)});
          if(opts.newProject||opts.rotate)currentUrl='https://chatgpt.com/c/new-project';},
        keyboard:{insertText:async s=>actions.push(['insert',s])},
        evaluate:async(fn,arg)=>fn.toString().includes('busy:')?{busy:!!opts.busy,draft:opts.draft||''}:fn(arg),
        waitForFunction:async(fn,arg,{timeout})=>{
          for(let i=0;i<(opts.maxPolls||12);i++,polls++){
            for(const e of opts.events||[])if(e.at===polls){if(e.rows)rows=e.rows;if('stop' in e)stop=e.stop;}
            const r=fn(arg);
            if(r){actions.push(['waited',polls]);return r;}
          }
          actions.push(['timeout',polls]);
          throw new Error('TimeoutError: waitForFunction '+timeout+'ms exceeded');
        },
        url:async()=>currentUrl};
      globalThis.taskSpace=async name=>({
        tabs:async()=>opts.noTabs?[]:[{url:base,label:'p2'},{url:'https://chatgpt.com/',label:'p3'}],
        page:label=>{actions.push(['reuse',label]);return page;},
        newPage:async()=>{actions.push(['new']);return page;}
      });
      globalThis.CONSULT_ARGS=${JSON.stringify(args)};
      process.on('exit',()=>console.log('ACTIONS '+JSON.stringify(actions)));
    `;
    const entry = join(dir, 'run.mjs');
    writeFileSync(entry, prelude + '\n' + script);
    const proc = spawnSync(process.execPath, [entry], {encoding:'utf8'});
    const line = proc.stdout.split('\n').find(l=>l.startsWith('ACTIONS '));
    return {code:proc.status,error:proc.stderr,actions:JSON.parse(line.slice(8)),
      journal:existsSync(journal)?JSON.parse(readFileSync(journal,'utf8')):null};
  } finally { rmSync(dir, {recursive:true,force:true}); }
}
const sends = r => r.actions.filter(a=>a[0]==='insert'||a[0]==='click');
const collect = (extra = {}) => run({collect:true, progress:{phase:'sent',project_id:pid,url,before:0}, ...extra});

test('known project reuses exact conversation and pairs answer to request, not last answer', () => {
  const r=run({rows:olderHistory(3)});
  assert.equal(r.code,0,r.error);
  assert.deepEqual(r.actions[0],['reuse','p2']);
  assert.equal(r.actions.filter(a=>a[0]==='new').length,0);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
  assert.equal(r.journal.url,url);
  assert.match(r.actions.find(a=>a[0]==='insert')[1],new RegExp(key));
});
test('new project never adopts another project home tab and waits for its real /c/ URL', () => {
  const r=run({newProject:true});
  assert.equal(r.code,0,r.error);
  assert.deepEqual(r.actions[0],['new']);
  assert.match(r.actions.find(a=>a[0]==='insert')[1],new RegExp(pid));
  assert.equal(r.journal.url,'https://chatgpt.com/c/new-project');
});
test('busy conversation and existing draft are not overwritten', () => {
  for(const opts of [{busy:true},{draft:'human draft'}]) {
    const r=run(opts);
    assert.notEqual(r.code,0);
    assert.equal(sends(r).length,0);
  }
});
test('send failure leaves durable uncertainty before click, never blind retry', () => {
  const r=run({failSend:true});
  assert.notEqual(r.code,0);
  assert.equal(r.journal.phase,'dispatching');
  const retry=run({progress:r.journal});
  assert.notEqual(retry.code,0);
  assert.equal(retry.actions.length,0);
});
test('collect only retrieves the bound answer and never sends another question', () => {
  const r=collect({rows:boundTurn});
  assert.equal(r.code,0,r.error);
  assert.equal(sends(r).length,0);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
});
test('collect after reopening with less rendered history still finds the finished answer', () => {
  // Sent when 100 assistant turns were rendered; the reopened tab only renders the latest few.
  const r=collect({progress:{phase:'sent',project_id:pid,url,before:100},
    rows:[...olderHistory(8),...boundTurn]});
  assert.equal(r.code,0,r.error);
  assert.equal(sends(r).length,0);
  assert.equal(r.journal.phase,'done');
  assert.equal(r.journal.answer,'BOUND_ANSWER');
});
test('collect with an unchanged assistant count accepts the answer already present', () => {
  const rows=[...olderHistory(1),['user',marker+' question'],['assistant','BOUND_ANSWER',true]];
  const r=collect({progress:{phase:'sent',project_id:pid,url,before:2},rows});
  assert.equal(r.code,0,r.error);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
});
test('a later unrelated finished answer is never taken while the bound one is partial', () => {
  const rows=[['user',marker+' question'],['assistant','BOUND_PARTIAL',false],
    ['user','manual later question'],['assistant','WRONG_LATER_ANSWER',true]];
  const r=collect({rows});
  assert.notEqual(r.code,0);
  assert.equal(sends(r).length,0);
  assert.equal(r.journal.phase,'sent');
  assert.equal(r.journal.answer,undefined);
});
test('a streaming answer is collected only once its turn is complete', () => {
  const r=collect({rows:[['user',marker+' question'],['assistant','BOUND_',false]],stop:true,
    events:[{at:2,rows:[['user',marker+' question'],['assistant','BOUND_ANSWER',true]]},{at:4,stop:false}]});
  assert.equal(r.code,0,r.error);
  assert.deepEqual(r.actions.find(a=>a[0]==='waited'),['waited',4]);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
});
test('unknown delivery collect never clicks or sends, whether or not the marker arrived', () => {
  const unknown={phase:'dispatching',project_id:pid,url,before:5};
  const missing=collect({progress:unknown,rows:olderHistory(5)});
  assert.notEqual(missing.code,0);
  assert.equal(sends(missing).length,0);
  assert.equal(missing.journal.phase,'dispatching');
  assert.equal(missing.journal.answer,undefined);
  const arrived=collect({progress:unknown,rows:[...olderHistory(2),...boundTurn]});
  assert.equal(arrived.code,0,arrived.error);
  assert.equal(sends(arrived).length,0);
  assert.equal(arrived.journal.answer,'BOUND_ANSWER');
});
test('completed browser receipt is replayed without any page operations', () => {
  const r=run({progress:{phase:'done',project_id:pid,url,answer:'saved'}});
  assert.equal(r.code,0,r.error);
  assert.equal(r.actions.length,0);
});
test('corrupt or wrong-project journal is not silently ignored', () => {
  const r=run({progress:{phase:'sent',project_id:'01M1Y75JS1G8PZ4EHF6Y98AFB1',url,before:0}});
  assert.notEqual(r.code,0);
  assert.equal(r.actions.length,0);
});
test('a partial response without its completed-turn controls is not accepted', () => {
  const r=run({afterSend:[['user',marker+' question'],['assistant','PARTIAL',false]]});
  assert.notEqual(r.code,0);
  assert.notEqual(r.journal.phase,'done');
  const stopping=collect({rows:boundTurn,stop:true});
  assert.notEqual(stopping.code,0);
  assert.equal(stopping.journal.phase,'sent');
});

test('換一串時重用這個 project 自己的分頁，不是再開一個', () => {
  const r = run({rotate: true});
  assert.equal(r.code, 0, r.error);
  assert.ok(r.actions.some(a => a[0] === 'reuse' && a[1] === 'p2'), JSON.stringify(r.actions));
  assert.ok(!r.actions.some(a => a[0] === 'new'), '不該再開一個分頁：一個 project 一個分頁');
  assert.deepEqual(r.actions.find(a => a[0] === 'goto'), ['goto', 'https://chatgpt.com/'], '導到新對話');
  assert.equal(r.journal.url, 'https://chatgpt.com/c/new-project', '記下來的是新那一串');
  // 新的一串要重新自我介紹：不然那串裡沒有任何東西說得出它屬於哪個 project。
  assert.ok(r.actions.some(a => a[0] === 'insert' && a[1].includes(`OB｜AM｜${pid}`)), JSON.stringify(r.actions));
});

test('沒有舊分頁可重用時才開新分頁', () => {
  const r = run({rotate: true, noTabs: true});
  assert.equal(r.code, 0, r.error);
  assert.ok(r.actions.some(a => a[0] === 'new'), JSON.stringify(r.actions));
});
