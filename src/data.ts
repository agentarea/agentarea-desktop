/**
 * Session model. A session is a chat run either by a local agent CLI (Claude
 * Code or Codex) or by a cloud AgentArea agent. Local sessions get exactly the
 * MCP servers and skills picked for them: the pick lives on the control plane as
 * an AgentArea client (one per session), which serves the whole set through a
 * single MCP endpoint. Cloud agents bring their own tools.
 */

export type Runner = 'claude' | 'codex' | 'cloud';

/** An MCP server instance or a skill from the workspace catalog. */
export interface CatalogItem {
  id: string;
  name: string;
  description?: string;
  /** logo URLs to try in order (registry icon, then the host's favicon) */
  icons?: string[];
  /** skills: a read-only built-in from the platform catalog, shared by every workspace */
  builtin?: boolean;
}

/** `note`: a muted status line from the app itself (e.g. "Stopped"). */
export type MessageRole = 'user' | 'assistant' | 'tool' | 'error' | 'note';

export interface Message {
  id: string;
  role: MessageRole;
  text: string;
  /** tool messages: the tool name; `text` holds its input */
  tool?: string;
  /** tool messages: id that links the call to its result */
  callId?: string;
  /** tool messages: the result, once it arrives */
  result?: string;
}

export interface Session {
  id: string;
  /** fixed once the first message is sent */
  runner: Runner;
  /** cloud runner: the AgentArea agent */
  agent: CatalogItem | null;
  title: string;
  createdAt: number;
  /** AgentArea client holding this session's MCP + skills; null when offline */
  clientId: string | null;
  mcpEndpointUrl: string | null;
  mcpIds: string[];
  skillIds: string[];
  messages: Message[];
  /** what the next turn continues: a CLI session/thread id, or the cloud task id */
  resumeId: string | null;
  /** folder the local agent works in; null = the thread's own ~/AgentArea/<id> */
  cwd: string | null;
  /** files dropped into the composer, sent with the next message */
  attachments: string[];
  /** local runners: model id / alias; null = the CLI's default */
  model: string | null;
  /** local runners: reasoning effort; null = the model's default */
  effort: string | null;
  /** how many messages the user has seen; newer agent replies make it "needs you" */
  readCount: number;
}
