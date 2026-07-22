import { describe, expect, it } from 'vitest'
import { shouldSubmitPromptOnEnter } from './App'

describe('shouldSubmitPromptOnEnter', () => {
  it('submits on Enter', () => {
    expect(shouldSubmitPromptOnEnter('Enter', false, false)).toBe(true)
  })

  it('keeps Ctrl + Enter for a newline', () => {
    expect(shouldSubmitPromptOnEnter('Enter', true, false)).toBe(false)
  })

  it('does not submit while an IME composition is active', () => {
    expect(shouldSubmitPromptOnEnter('Enter', false, true)).toBe(false)
  })

  it('ignores keys other than Enter', () => {
    expect(shouldSubmitPromptOnEnter('a', false, false)).toBe(false)
  })
})
