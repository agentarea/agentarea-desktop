/**
 * Bridge to the Rust thread processes (runners.rs): each local thread keeps
 * one `claude` or `codex app-server` process alive between turns. `run_agent`
 * starts a turn; the process answers with `agent-line` (stdout to parse),
 * `agent-approval` / `agent-approval-resolved` (things waiting on the user)
 * and `agent-turn-end`.
 */
import type { Message } from '../data';

export type LocalRunner = 'claude' | 'codex';

export interface AgentLine {
  session_id: string;
  line: string;
}

/** Something the agent wants to do that needs the user's yes. */
export interface Approval {
  session_id: string;
  id: string;
  kind: 'shell' | 'edit' | 'tool' | 'permissions';
  /** e.g. "Run a command" */
  title: string;
  /** the command, the files, or the tool's input */
  detail: string;
  reason: string | null;
  /** whether "Allow for this session" is on offer */
  can_session: boolean;
}

export type Decision = 'allow' | 'allow_session' | 'deny';

export interface TurnEnd {
  session_id: string;
  /** set when the turn failed (or its process died) */
  error: string | null;
  /** the user pressed Stop */
  interrupted: boolean;
}

export async function runLocal(args: {
  runner: LocalRunner;
  sessionId: string;
  prompt: string;
  resumeId: string | null;
  mcpUrl: string | null;
  token: string | null;
  cwd: string | null;
  model: string | null;
  effort: string | null;
  /** cloud data sources: the workspace (header ref + display name) and API base */
  workspace: string | null;
  workspaceName: string | null;
  apiBase: string;
  /** the thread's picked MCP servers + skills; a change restarts its process */
  toolsKey: string;
}): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('run_agent', { ...args });
}

export async function approveLocal(sessionId: string, id: string, decision: Decision): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('agent_approve', { sessionId, id, decision });
}

export async function stopLocal(sessionId: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('agent_interrupt', { sessionId });
}

export async function closeLocal(sessionId: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('agent_close', { sessionId });
}

export async function onLocal(handlers: {
  line: (e: AgentLine) => void;
  approval: (e: Approval) => void;
  resolved: (e: { session_id: string; id: string }) => void;
  turnEnd: (e: TurnEnd) => void;
}): Promise<() => void> {
  const { listen } = await import('@tauri-apps/api/event');
  const offs = await Promise.all([
    listen<AgentLine>('agent-line', (e) => handlers.line(e.payload)),
    listen<Approval>('agent-approval', (e) => handlers.approval(e.payload)),
    listen<{ session_id: string; id: string }>('agent-approval-resolved', (e) => handlers.resolved(e.payload)),
    listen<TurnEnd>('agent-turn-end', (e) => handlers.turnEnd(e.payload)),
  ]);
  return () => offs.forEach((off) => off());
}

type Out = Omit<Message, 'id'>;

/**
 * What one line means for the chat: a CLI session id to resume later, new
 * messages, results for tool calls shown earlier (matched by callId), and
 * streamed text for the reply being written (`delta`).
 */
export interface Parsed {
  resumeId?: string;
  messages: Out[];
  results?: { callId: string; text: string }[];
  delta?: string;
}

const str = (v: unknown) => (typeof v === 'string' ? v : JSON.stringify(v ?? {}));

const tool = (name: string, input: unknown, callId?: string, result?: unknown): Out => ({
  role: 'tool',
  tool: name.replace(/^mcp__agentarea(_secrets|_data)?__/, ''),
  text: str(input),
  callId,
  result: result === undefined ? undefined : str(result),
});

/** Tool results come as a string or a list of content blocks. */
function resultText(content: unknown): string {
  if (Array.isArray(content))
    return content.map((c: any) => (c?.type === 'text' ? c.text : `[${c?.type ?? 'content'}]`)).join('\n');
  return str(content);
}

/** `/bin/zsh -lc 'ls -la'` → `ls -la`. */
function unwrapShell(command: string): string {
  const m = /^(?:\/bin\/)?(?:zsh|bash|sh) -l?c '([^']*)'$/.exec(command);
  return m ? m[1] : command;
}

/** Claude Code stream-json (with partial messages). Turn ends and errors come as `agent-turn-end`. */
function parseClaude(ev: any): Parsed {
  if (ev.type === 'system' && ev.subtype === 'init') return { resumeId: ev.session_id, messages: [] };
  if (ev.type === 'stream_event' && ev.event?.delta?.type === 'text_delta')
    return { messages: [], delta: ev.event.delta.text };
  if (ev.type === 'assistant')
    return {
      messages: (ev.message?.content ?? []).flatMap((c: any): Out[] => {
        if (c.type === 'text' && c.text?.trim()) return [{ role: 'assistant', text: c.text }];
        if (c.type === 'tool_use') return [tool(String(c.name), c.input, c.id)];
        return [];
      }),
    };
  if (ev.type === 'user')
    return {
      messages: [],
      results: (ev.message?.content ?? [])
        .filter((c: any) => c.type === 'tool_result')
        .map((c: any) => ({ callId: c.tool_use_id, text: resultText(c.content) })),
    };
  return { messages: [] };
}

/** Codex app-server notifications (JSON-RPC). Turn ends and errors come as `agent-turn-end`. */
function parseCodex(ev: any): Parsed {
  const p = ev.params ?? {};
  switch (ev.method) {
    case 'thread/started':
      return { resumeId: p.thread?.id, messages: [] };
    case 'item/agentMessage/delta':
      return { messages: [], delta: p.delta ?? '' };
    case 'item/started': {
      const it = p.item ?? {};
      if (it.type === 'commandExecution') return { messages: [tool('shell', unwrapShell(it.command ?? ''), it.id)] };
      if (it.type === 'mcpToolCall') return { messages: [tool(String(it.tool ?? 'tool'), it.arguments, it.id)] };
      if (it.type === 'fileChange')
        // raw changes ({path, kind, diff}[]) — the view renders them as "Edited x" rows
        return { messages: [tool('fileChange', it.changes ?? [], it.id)] };
      if (it.type === 'webSearch') return { messages: [tool('web_search', it.query ?? '', it.id)] };
      return { messages: [] };
    }
    case 'item/completed': {
      const it = p.item ?? {};
      const done = (text: string): Parsed => ({ messages: [], results: [{ callId: it.id, text }] });
      switch (it.type) {
        case 'agentMessage':
          return { messages: it.text?.trim() ? [{ role: 'assistant', text: it.text }] : [] };
        case 'commandExecution':
          return done(it.aggregatedOutput ?? (it.status === 'declined' ? 'Declined' : `exit ${it.exitCode ?? '?'}`));
        case 'mcpToolCall':
          return done(it.error ? str(it.error.message ?? it.error) : resultText(it.result?.content ?? it.result));
        case 'fileChange':
          return done(it.status ?? 'done');
        case 'webSearch':
          return done('done');
        default:
          return { messages: [] };
      }
    }
    default:
      return { messages: [] };
  }
}

export function parseLine(runner: LocalRunner, line: string): Parsed {
  try {
    const ev = JSON.parse(line);
    return runner === 'claude' ? parseClaude(ev) : parseCodex(ev);
  } catch {
    return { messages: [] };
  }
}
