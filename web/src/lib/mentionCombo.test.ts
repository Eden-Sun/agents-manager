import test from 'node:test'
import assert from 'node:assert/strict'
import { mentionComboAttrs, mentionOptionId } from './mentionCombo.ts'

// 輸入框旁的 @ 候選是 listbox，但輸入框本身沒有 combobox 語意，螢幕閱讀器聽不到選了哪一項、清單有沒有開。
test('清單開著：輸入框是 combobox，aria-activedescendant 指到目前那一項', () => {
  const a = mentionComboAttrs({ open: true, listId: 'L', activeIdx: 1, count: 3 })
  assert.equal(a.role, 'combobox')
  assert.equal(a['aria-expanded'], true)
  assert.equal(a['aria-controls'], 'L')
  assert.equal(a['aria-activedescendant'], mentionOptionId('L', 1))
  assert.equal(a['aria-autocomplete'], 'list')
})

test('清單關著或沒有候選：不指 activedescendant、aria-expanded=false', () => {
  assert.equal(mentionComboAttrs({ open: false, listId: 'L', activeIdx: 0, count: 3 })['aria-activedescendant'], undefined)
  const empty = mentionComboAttrs({ open: true, listId: 'L', activeIdx: 0, count: 0 })
  assert.equal(empty['aria-expanded'], false)
  assert.equal(empty.role, undefined)
  assert.equal(empty['aria-activedescendant'], undefined)
})
