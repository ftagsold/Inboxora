function messageIds(messages: unknown): string[] {
  if (!Array.isArray(messages)) return [];

  const ids: string[] = [];
  for (const message of messages) {
    if (typeof message !== 'object' || message === null || !('id' in message)) continue;
    const { id } = message;
    if (typeof id === 'string' && id) ids.push(id);
  }
  return ids;
}

export function initialConversationTarget(messages: unknown, targetLogicalMessageId: unknown): string | null {
  const ids = messageIds(messages);
  if (typeof targetLogicalMessageId === 'string' && targetLogicalMessageId && ids.includes(targetLogicalMessageId)) {
    return targetLogicalMessageId;
  }

  const newestId = ids.at(-1);
  return newestId === undefined ? null : newestId;
}

/** Every message of a conversation opens expanded, the target first. */
export function initialConversationExpansion(messages: unknown, targetLogicalMessageId: unknown): Set<string> {
  const target = initialConversationTarget(messages, targetLogicalMessageId);
  if (target === null) return new Set();
  return new Set([target, ...messageIds(messages)]);
}

/** The order expanded bodies load in: the target, then its neighbours outwards. */
export function bodyLoadOrder(expanded: Iterable<string>, messages: unknown, targetLogicalMessageId: string | null): string[] {
  const open = new Set(expanded);
  const ids = messageIds(messages).filter(id => open.has(id));
  for (const id of open) if (!ids.includes(id)) ids.push(id);
  const at = targetLogicalMessageId ? ids.indexOf(targetLogicalMessageId) : -1;
  if (at < 0) return ids;
  const out = [ids[at]];
  for (let d = 1; out.length < ids.length; d += 1) {
    if (at + d < ids.length) out.push(ids[at + d]);
    if (at - d >= 0) out.push(ids[at - d]);
  }
  return out;
}

export function toggleConversationExpansion(expanded: ReadonlySet<string>, logicalMessageId: string): Set<string> {
  const next = new Set(expanded);
  if (next.has(logicalMessageId)) next.delete(logicalMessageId);
  else next.add(logicalMessageId);
  return next;
}
