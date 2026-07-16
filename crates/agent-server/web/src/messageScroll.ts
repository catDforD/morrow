const MESSAGE_BOTTOM_THRESHOLD_PX = 48

export interface MessageScrollMetrics {
  scrollTop: number
  scrollHeight: number
  clientHeight: number
}

export function isMessageScrollNearBottom(
  scroller: MessageScrollMetrics,
): boolean {
  const remaining =
    scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop
  return remaining <= MESSAGE_BOTTOM_THRESHOLD_PX
}

export function scrollMessageListToBottom(
  scroller: MessageScrollMetrics,
): void {
  scroller.scrollTop = Math.max(
    0,
    scroller.scrollHeight - scroller.clientHeight,
  )
}
