/**
 * Local half of the unified inbox: threads that need the user — an agent
 * waiting on an approval, or one that replied (or failed) since they last
 * looked. Derived from thread state, so it needs no storage of its own;
 * `readCount` moves when a thread is viewed.
 */
import type { Session } from '../data';
import type { Approval } from './local';

export interface LocalInboxItem {
  session: Session;
  kind: 'approval' | 'reply' | 'failed';
  /** last agent message, or what the agent wants to do */
  preview: string;
  approval?: Approval;
}

export function localInbox(
  sessions: Session[],
  running: Record<string, boolean>,
  approvals: Record<string, Approval[]>,
): LocalInboxItem[] {
  const asks = sessions.flatMap((s) =>
    (approvals[s.id] ?? []).map((a): LocalInboxItem => ({ session: s, kind: 'approval', preview: a.detail, approval: a })),
  );
  const replies = sessions.flatMap((s): LocalInboxItem[] => {
    if (running[s.id] || s.messages.length <= s.readCount) return [];
    const last = s.messages[s.messages.length - 1];
    if (last.role !== 'assistant' && last.role !== 'error') return [];
    return [{ session: s, kind: last.role === 'error' ? 'failed' : 'reply', preview: last.text }];
  });
  return [...asks, ...replies];
}
