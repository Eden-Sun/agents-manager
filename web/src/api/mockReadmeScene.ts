import { L } from './mockText'
import type { TWO_ASK_QUESTIONS } from './mockTwoAsk'

export const readmeAsk = (): typeof TWO_ASK_QUESTIONS => [
  {
    question: L('PR #412 在 staging 上 429 歸零，要合併並上線嗎？', 'PR #412 shows zero 429s on staging. Merge and ship it?'),
    header: L('合併 #412', 'Merge #412'),
    multiSelect: false,
    options: [
      { label: L('合併並上線 (Recommended)', 'Merge and ship (Recommended)'), description: L('214 個測試全綠，p95 只多 24 ms。', 'All 214 tests pass; p95 latency is up only 24 ms.') },
      { label: L('先不要', 'Hold'), description: L('我想先自己在 staging 試一下。', 'I want to try it on staging myself first.') },
    ],
  },
  {
    question: L('接下來做哪幾件？', 'Which follow-ups should I do next?'),
    header: L('後續', 'Next'),
    multiSelect: true,
    options: [
      { label: L('加重試上限', 'Cap the backoff'), description: L('60 秒後停止重試，上游掛掉時不會卡住 worker。', 'Stop retrying after 60 s so a dead upstream cannot pin a worker.') },
      { label: L('補文件', 'Document the new setting'), description: L('把 RETRY_MAX_MS 寫進 README 與 .env.example。', 'Add RETRY_MAX_MS to the README and .env.example.') },
    ],
  },
]

export const readmeChat = (): ['user' | 'assistant', string][] => [
  [
    'user',
    L(
      '結帳服務在尖峰時會被付款 API 回 429，可以幫忙加重試嗎？',
      'Checkout keeps getting 429s from the payments API at peak load. Can you add retries?',
    ),
  ],
  [
    'assistant',
    L(
      '找到了：`PaymentsClient.charge()` 遇到 429 只試一次就放棄。\n\n已在 `fix/payments-retry` 改好：\n- 429／503 以指數退避加抖動重試（最多 3 次）\n- 有 `Retry-After` 就照它等\n- 新增 `charge_retries_on_429` 測試與 200 rps 壓測\n\n```\n✓ 214 passed · 0 failed · 38.2s\n```\n已開 **PR #412**。',
      'Found it: `PaymentsClient.charge()` gives up after a single 429.\n\nFixed on `fix/payments-retry`:\n- Retry 429 and 503 with exponential backoff and jitter (up to 3 attempts)\n- Honor `Retry-After` when the API sends it\n- New `charge_retries_on_429` test plus a 200 rps load test\n\n```\n✓ 214 passed · 0 failed · 38.2s\n```\nOpened **PR #412**.',
    ),
  ],
  ['user', L('很好。部署到 staging，觀察 10 分鐘錯誤率。', 'Nice. Deploy it to staging and watch the error rate for 10 minutes.')],
  [
    'assistant',
    L(
      'staging 已切到 `fix/payments-retry`。10 分鐘、約 180 rps：\n\n| | 之前 | 之後 |\n|---|---|---|\n| 客戶端看到的 429 | 3.1% | **0** |\n| p95 延遲 | 412 ms | 436 ms |',
      'Staging is on `fix/payments-retry`. Ten minutes at about 180 rps:\n\n| | Before | After |\n|---|---|---|\n| 429s seen by clients | 3.1% | **0** |\n| p95 latency | 412 ms | 436 ms |',
    ),
  ],
]

export const readmeDone = (): string =>
  L('依賴升級完成：12 個套件更新、無破壞性變更，CI 全綠。', 'Dependency bump done: 12 packages updated, no breaking changes, CI is green.')
