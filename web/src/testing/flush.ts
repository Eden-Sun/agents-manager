import { settle } from './domHarness'

export async function flush(rounds = 8): Promise<void> {
  for (let i = 0; i < rounds; i++) await settle()
}
