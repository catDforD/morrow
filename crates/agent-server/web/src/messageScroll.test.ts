import { describe, expect, it } from 'vitest'
import {
  isMessageScrollNearBottom,
  scrollMessageListToBottom,
} from './messageScroll'

describe('message scroll following', () => {
  it('follows the real bottom and its nearby threshold', () => {
    expect(
      isMessageScrollNearBottom({
        scrollTop: 600,
        scrollHeight: 1_500,
        clientHeight: 900,
      }),
    ).toBe(true)
    expect(
      isMessageScrollNearBottom({
        scrollTop: 552,
        scrollHeight: 1_500,
        clientHeight: 900,
      }),
    ).toBe(true)
  })

  it('stops following when trailing composer space remains', () => {
    expect(
      isMessageScrollNearBottom({
        scrollTop: 401,
        scrollHeight: 1_500,
        clientHeight: 900,
      }),
    ).toBe(false)
  })

  it('scrolls to the container maximum including trailing padding', () => {
    const scroller = {
      scrollTop: 401,
      scrollHeight: 1_500,
      clientHeight: 900,
    }

    scrollMessageListToBottom(scroller)

    expect(scroller.scrollTop).toBe(600)
  })
})
