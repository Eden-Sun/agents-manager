import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * codex 登入會先刪掉那個帳號原本的登入（`lib/codexLoginGuard`）：已登入的身份要重新登入之前，這裡先問。
 * 所有會走 codex 登入的入口（身份列、額度、登入提示條、Bot 設定、新增身份、預設帳號的 shell）都經過 `confirmCodexLoginReplace`。
 */
export function CodexLoginReplaceDialog() {
  const t = useT()
  const ask = useStore((s) => s.codexLoginAsk)
  const answer = useStore((s) => s.answerCodexLoginAsk)
  return (
    <ConfirmDialog
      open={ask !== null}
      title={t('重新登入 codex？')}
      body={t('開始後這個帳號原本的登入會立即失效，用這個身份的 codex bot 要等你完成登入才能繼續。')}
      confirmLabel={t('開始登入')}
      cancelLabel={t('先不要')}
      danger
      onConfirm={() => answer(true)}
      onCancel={() => answer(false)}
    />
  )
}
