export function mentionOptionId(listId: string, i: number): string {
  return `${listId}-opt-${i}`
}

/** @ 候選清單開著時，輸入框要宣告成 combobox 並指向目前那一項（焦點留在輸入框，靠 aria-activedescendant）。 */
export function mentionComboAttrs({ open, listId, activeIdx, count }: { open: boolean; listId: string; activeIdx: number; count: number }) {
  const expanded = open && count > 0
  return {
    // 平常打字不宣告成 combobox（否則每次聚焦都被念一次）；候選開著才是。
    role: expanded ? ('combobox' as const) : undefined,
    'aria-autocomplete': 'list' as const,
    'aria-expanded': expanded,
    'aria-controls': expanded ? listId : undefined,
    'aria-activedescendant': expanded ? mentionOptionId(listId, activeIdx) : undefined,
  }
}
