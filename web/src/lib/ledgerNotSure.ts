import { useT } from './i18n'

/** 「不知道」寫進下一步的複查指示（介面語言）。 */
export function useNotSureNext(): string {
  const t = useT()
  return t('使用者不確定是否完成：請複查完成度並附驗證證據（commit、測試、截圖或實測結果）後再交件')
}
