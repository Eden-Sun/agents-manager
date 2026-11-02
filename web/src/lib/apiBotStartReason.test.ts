import { test } from 'node:test'
import assert from 'node:assert/strict'
import { apiBotStartReasonText } from './apiBotStartReason'

test('每個已知原因都有中文說明，未知原因回 null', () => {
  for (const r of ['api_bot_disabled', 'api_bot_turn_cost_cap_missing', 'api_bot_turn_cost_cap_too_low', 'api_bot_config_invalid', 'api_bot_sandbox_unavailable', 'api_bot_workspace_unavailable', 'api_bot_key_unavailable', 'api_bot_host_offline', 'api_native_prompt_unconfirmed', 'api_native_start_unconfirmed', 'api_bot_stop_unconfirmed:busy']) {
    assert.ok(apiBotStartReasonText(r), r)
  }
  assert.match(apiBotStartReasonText('api_bot_disabled') ?? '', /api_bot/)
  assert.equal(apiBotStartReasonText(undefined), null)
})
