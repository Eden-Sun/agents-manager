import { test } from 'node:test'
import assert from 'node:assert/strict'
import { apiBotStartReasonText } from './apiBotStartReason'

test('每個已知原因都有中文說明，未知原因回 null', () => {
  for (const r of [
    'api_bot_disabled',
    'api_bot_turn_cost_cap_missing',
    'api_bot_turn_cost_cap_too_low',
    'api_bot_config_invalid',
    'api_bot_sandbox_unavailable',
    'api_bot_workspace_unavailable',
    'api_bot_key_unavailable',
    'api_bot_host_offline',
    'api_native_prompt_unconfirmed',
    'api_native_start_unconfirmed',
    'api_bot_stop_unconfirmed:busy',
    'api_model_changed',
    'api_context_unsettled',
    'api_session_expired',
    'api_checkpoint_too_large',
    'api_receipt_unsettled',
  ]) {
    assert.ok(apiBotStartReasonText(r), r)
  }
  assert.match(apiBotStartReasonText('api_bot_disabled') ?? '', /api_bot/)
  assert.equal(apiBotStartReasonText(undefined), null)
})

test('openai-api bot 缺單回合上限：講 OpenAI 的設定位置；claude-api 與沒給 kind 維持原句', () => {
  const oa = apiBotStartReasonText('api_bot_turn_cost_cap_missing', 'openai-api') ?? ''
  assert.match(oa, /OpenAI API/)
  assert.doesNotMatch(oa, /claude_api/)
  assert.match(oa, /openai_api\.api_bot/)
  const cl = apiBotStartReasonText('api_bot_turn_cost_cap_missing', 'claude-api') ?? ''
  assert.match(cl, /\[claude_api\.api_bot\]/)
  assert.equal(apiBotStartReasonText('api_bot_turn_cost_cap_missing'), cl, '沒給 kind 也是 Claude 原句')
  assert.match(apiBotStartReasonText('api_bot_key_unavailable', 'openai-api') ?? '', /\[openai_api\] key_file/)
  assert.match(apiBotStartReasonText('api_bot_config_invalid', 'openai-api') ?? '', /\[openai_api\.api_bot\]/)
  assert.match(apiBotStartReasonText('api_bot_key_unavailable', 'claude-api') ?? '', /\[claude_api\] key_file/)
})
