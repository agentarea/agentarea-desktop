/**
 * Delegating a thread to a cloud AgentArea agent.
 *
 * The first message creates a task (`POST /v1/agents/{id}/tasks/`, answered with
 * an SSE stream). After a reply the task stays alive for ~30 min waiting for a
 * follow-up, which goes in as a `queue_message` command to the same task; its
 * events arrive on `/events/stream`. Once the task has stopped waiting (409), a
 * new task is started — like the web chat, it carries no history of the old one.
 */
import { apiRequest } from './api';
import type { Message } from '../data';

type Out = Omit<Message, 'id'>;

/** Events after which the agent is waiting for the user (or done). */
const TURN_END = new Set([
  'task.completed',
  'task.awaiting_follow_up',
  'task.failed',
  'task.cancelled',
  'task_failed',
  'error',
  'input.request',
  'approval.request',
]);

/** The events stream replays history first; skip events already shown. */
const seen = new Map<string, Set<string>>();

export async function runCloud(args: {
  agentId: string;
  taskId: string | null;
  prompt: string;
  onTaskId: (taskId: string) => void;
  onMessages: (msgs: Out[]) => void;
}): Promise<void> {
  const base = `/v1/agents/${args.agentId}/tasks`;

  if (args.taskId) {
    const since = Date.now() - 2000;
    const res = await apiRequest(`${base}/${args.taskId}/command`, {
      method: 'POST',
      body: JSON.stringify({ command: 'queue_message', message: args.prompt }),
    });
    if (res.ok) {
      const stream = await apiRequest(`${base}/${args.taskId}/events/stream`);
      if (!stream.ok) throw new Error(`event stream failed (${stream.status})`);
      return consume(stream, args.taskId, since, args);
    }
    if (res.status !== 409) throw new Error(`follow-up failed (${res.status})`);
    // 409: the task is no longer waiting — fall through to a fresh task.
  }

  const res = await apiRequest(`${base}/`, {
    method: 'POST',
    body: JSON.stringify({
      description: args.prompt,
      parameters: { task_type: 'chat', interaction: { channel: 'desktop' } },
    }),
  });
  if (!res.ok) throw new Error(`create task failed (${res.status})`);
  return consume(res, null, 0, args);
}

async function consume(
  res: Response,
  taskId: string | null,
  since: number,
  args: { onTaskId: (id: string) => void; onMessages: (msgs: Out[]) => void },
): Promise<void> {
  if (!res.body) throw new Error('empty event stream');
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
  let buf = '';
  let sawText = false;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buf += value;
      let cut: number;
      while ((cut = buf.indexOf('\n\n')) >= 0) {
        const frame = buf.slice(0, cut);
        buf = buf.slice(cut + 2);
        const { type, data } = parseFrame(frame);
        if (!type) continue;
        if (type === 'task_created' && data.task_id) {
          taskId = String(data.task_id);
          args.onTaskId(taskId);
          continue;
        }
        if (taskId && data.event_id) {
          const ids = seen.get(taskId) ?? new Set<string>();
          seen.set(taskId, ids);
          if (ids.has(data.event_id)) continue;
          ids.add(data.event_id);
        }
        if (since && data.timestamp && Date.parse(data.timestamp) < since) continue;
        const d = { ...(data.data ?? {}), ...data };
        const msgs = toMessages(type, d, sawText);
        if (msgs.some((m) => m.role === 'assistant')) sawText = true;
        if (msgs.length) args.onMessages(msgs);
        if (TURN_END.has(type)) return;
      }
    }
  } finally {
    reader.cancel().catch(() => {});
  }
}

function parseFrame(frame: string): { type: string | null; data: any } {
  let type: string | null = null;
  const data: string[] = [];
  for (const line of frame.split('\n')) {
    if (line.startsWith('event:')) type = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
  }
  try {
    const parsed = data.length ? JSON.parse(data.join('\n')) : {};
    return { type: type ?? parsed.event_type ?? null, data: parsed };
  } catch {
    return { type: null, data: {} };
  }
}

function toMessages(type: string, d: any, sawText: boolean): Out[] {
  switch (type) {
    case 'llm.call.completed':
      return d.content?.trim() ? [{ role: 'assistant', text: d.content }] : [];
    case 'tool.call':
      return [
        {
          role: 'tool',
          tool: String(d.tool_name ?? 'tool'),
          text: typeof d.arguments === 'string' ? d.arguments : JSON.stringify(d.arguments ?? {}),
        },
      ];
    case 'task.completed':
      // The answer usually already came as llm.call.completed.
      return !sawText && d.result ? [{ role: 'assistant', text: String(d.result) }] : [];
    case 'input.request':
      return [{ role: 'assistant', text: d.question ?? 'The agent needs more input.' }];
    case 'approval.request':
      return [
        { role: 'error', text: `Waiting for approval to run ${d.tool_name ?? 'a tool'} — approve it in AgentArea.` },
      ];
    case 'task.failed':
    case 'task_failed':
    case 'error':
      return [{ role: 'error', text: String(d.error ?? 'Task failed') }];
    case 'task.cancelled':
      return [{ role: 'error', text: 'Task cancelled' }];
    default:
      return [];
  }
}
