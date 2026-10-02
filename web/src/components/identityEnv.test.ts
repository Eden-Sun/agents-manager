import test from 'node:test'
import assert from 'node:assert/strict'
import { checkEnvText, envDisplayText, parseEnvText } from './identityEnv'

test('一行打錯（少了 =、名字不合法、重複）不能靜靜被丟掉：丟掉的話身份的 env 變成空的＝悄悄用了預設帳號', () => {
  const r = checkEnvText('CLAUDE_CONFIG_DIR ~/.claude-cc1\nBAD NAME=1\n9LIVES=x\nA=1\nA=2\n=novalue')
  assert.deepEqual(r.env, { A: '2' }, '還是把合法的解出來（給預覽用）')
  assert.equal(r.errors.length, 5, r.errors.join(' | '))
  assert.ok(r.errors.some((e) => e.includes('第 1 行') && e.includes('=')), '少了 =：指出第幾行')
  assert.ok(r.errors.some((e) => e.includes('第 2 行') && e.includes('BAD NAME')), '名字不合法')
  assert.ok(r.errors.some((e) => e.includes('第 3 行') && e.includes('9LIVES')), '數字開頭')
  assert.ok(r.errors.some((e) => e.includes('第 5 行') && e.includes('A')), '重複的 key')
  assert.ok(r.errors.some((e) => e.includes('第 6 行')), '空的名字')
})

test('合法的 env 沒有任何錯誤；空行與 # 註解照舊略過；值裡的 = 保留', () => {
  const r = checkEnvText('# 註解\n\nCLAUDE_CONFIG_DIR=$HOME/.claude-cc1\nURL=https://x/?a=b')
  assert.deepEqual(r.errors, [])
  assert.deepEqual(r.env, { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc1', URL: 'https://x/?a=b' })
  assert.deepEqual(parseEnvText('A=1'), { A: '1' }, '舊的 parseEnvText 行為不變')
})

test('畫面上顯示 env 時，key／token／secret／password 類的值遮起來（列表與 tooltip 都會被截圖、被旁邊的人看到）', () => {
  const shown = envDisplayText({
    CLAUDE_CONFIG_DIR: '/home/u/.claude-cc1',
    ANTHROPIC_API_KEY: 'sk-ant-api03-abcdefghijklmnop',
    GH_TOKEN: 'ghp_secretsecret',
    DB_PASSWORD: 'hunter2',
    CLIENT_SECRET: 's3cr3t',
  })
  assert.ok(shown.includes('CLAUDE_CONFIG_DIR=/home/u/.claude-cc1'), '一般的值照顯示')
  for (const secret of ['sk-ant-api03-abcdefghijklmnop', 'ghp_secretsecret', 'hunter2', 's3cr3t']) {
    assert.ok(!shown.includes(secret), `不能出現 ${secret}：${shown}`)
  }
  assert.ok(shown.includes('ANTHROPIC_API_KEY=') && shown.includes('••'), '還看得出有這個 key、而且被遮了')
})
