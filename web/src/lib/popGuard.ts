export type PopDecision = 'close-dialog' | 'close-drawer' | 'stay' | 'navigate'

export function popDecision(input: {
  drawerOpen: boolean
  dialogOpen: boolean
  leavingSettings: boolean
  guardBlocks: () => boolean
}): PopDecision {
  if (input.dialogOpen) return 'close-dialog'
  if (input.drawerOpen) return 'close-drawer'
  if (input.leavingSettings && input.guardBlocks()) return 'stay'
  return 'navigate'
}
