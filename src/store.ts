/**
 * App store (Zustand) — matches Orca's `@/store` convention so copied components
 * that read `useAppStore(...)` slot in unchanged. Consolidates UI, auth and
 * control-plane connection state in one place.
 */
import { create } from 'zustand';
import { config, LOOPBACK_PORT, currentEnv, saveEnv, type Environment } from './lib/config';
import { discover, ensureClient, forgetClient } from './lib/oauth';
import {
  setSession,
  restoreSession,
  clearSession,
  dropSession,
  freshAccessToken,
  onSessionExpired,
} from './lib/session';
import {
  listAgents,
  listWorkspaces,
  setWorkspaceRef,
  listInbox,
  resolveEscalation,
  type CloudInboxItem,
  type Workspace,
  listMcpInstances,
  listSkills,
  createClient,
  deleteClient,
  addClientMember,
  removeClientMember,
} from './lib/api';
import { randomToken, challengeFromVerifier, buildAuthorizeUrl } from './lib/pkce';
import {
  runLocal,
  onLocal,
  parseLine,
  approveLocal,
  stopLocal,
  closeLocal,
  type Approval,
  type Decision,
  type LocalRunner,
} from './lib/local';
import { runCloud } from './lib/cloud';
import { loadCodexModels, type ModelOption } from './lib/models';
import type { CatalogItem, Message, Runner, Session } from './data';

export type Theme = 'light' | 'dark';
export type AuthStatus = 'unauthenticated' | 'authenticating' | 'authenticated';
export type ConnectionStatus = 'offline' | 'connecting' | 'connected';
export type MemberKind = 'mcp' | 'skill';

interface AppState {
  // UI
  settings: { theme: Theme };
  toggleTheme: () => void;

  // Environment (which AgentArea deployment); threads are kept per environment
  env: Environment;
  setEnv: (id: string) => void;

  // Auth
  auth: { status: AuthStatus; user: { email: string } | null; error: string | null };
  login: () => Promise<void>;
  logout: () => void;
  devComplete: () => void;

  // Control plane
  connection: ConnectionStatus;
  mcps: CatalogItem[];
  skills: CatalogItem[];
  agents: CatalogItem[];
  connect: () => Promise<void>;

  // Workspaces (orgs), switched like Slack; each keeps its own threads
  workspaces: Workspace[];
  workspaceId: string | null;
  setWorkspace: (id: string) => Promise<void>;

  /** Codex models from its cache (Claude's list is static) */
  codexModels: ModelOption[];

  // Inbox: cloud tasks needing attention (local ones are derived from threads)
  cloudInbox: CloudInboxItem[];
  loadInbox: () => Promise<void>;
  resolveCloud: (item: CloudInboxItem, approved: boolean) => Promise<void>;
  /** open a thread linked to a cloud task, to follow up on it */
  openCloudTask: (item: CloudInboxItem) => void;
  /** the user has now seen everything in this thread */
  markRead: (sessionId: string) => void;

  // Sessions
  sessions: Session[];
  selectedId: string | null;
  running: Record<string, boolean>;
  /** local threads: the reply being streamed, before it lands as a message */
  drafts: Record<string, string>;
  /** local threads: what the agent waits on the user for */
  approvals: Record<string, Approval[]>;
  approve: (sessionId: string, id: string, decision: Decision) => Promise<void>;
  /** Stop the running local turn */
  stop: (sessionId: string) => Promise<void>;
  select: (id: string | null) => void;
  newSession: () => Promise<void>;
  setRunner: (sessionId: string, runner: Runner, agent?: CatalogItem) => void;
  /** model / effort for the next turns of a local thread; null = default */
  setModel: (sessionId: string, model: string | null) => void;
  setEffort: (sessionId: string, effort: string | null) => void;
  /** open a folder picker; null clears back to the thread's own folder */
  pickFolder: (sessionId: string, clear?: boolean) => Promise<void>;
  /** set the folder directly (e.g. from recent folders); null = thread's own */
  setFolder: (sessionId: string, path: string | null) => void;
  recentFolders: string[];
  /** files dropped on a thread: attached to its next message */
  attachFiles: (sessionId: string, paths: string[]) => Promise<void>;
  removeAttachment: (sessionId: string, path: string) => void;
  /** attach files by path without copying (e.g. from a plugin's folder) */
  referenceFiles: (sessionId: string, paths: string[]) => void;
  deleteSession: (id: string) => Promise<void>;
  toggleMember: (sessionId: string, kind: MemberKind, id: string) => Promise<void>;
  send: (sessionId: string, text: string) => Promise<void>;
}

// Threads reference clients in one workspace of one deployment, so each keeps its own.
const workspaceKey = () => `aa.workspace:${config.apiBaseUrl}`;
function savedWorkspace(): string | null {
  try {
    return localStorage.getItem(workspaceKey());
  } catch {
    return null;
  }
}
let activeWorkspace: string | null = savedWorkspace();
const legacySessionsKey = () => `aa.sessions:${config.apiBaseUrl}`;
const sessionsKey = () => `aa.sessions:${config.apiBaseUrl}:${activeWorkspace ?? 'default'}`;

function loadSessions(): Session[] {
  try {
    // Threads saved before workspaces existed move into the first workspace opened.
    let stored = localStorage.getItem(sessionsKey());
    if (stored === null && activeWorkspace && localStorage.getItem(legacySessionsKey()) !== null) {
      stored = localStorage.getItem(legacySessionsKey());
      localStorage.setItem(sessionsKey(), stored ?? '[]');
      localStorage.removeItem(legacySessionsKey());
    }
    const raw: Partial<Session>[] = JSON.parse(stored ?? '[]');
    // Fill fields added after a session was saved.
    return raw.map(
      (s) =>
        ({
          runner: 'claude',
          agent: null,
          resumeId: null,
          cwd: null,
          attachments: [],
          model: null,
          effort: null,
          readCount: s.messages?.length ?? 0,
          ...s,
        }) as Session,
    );
  } catch {
    return [];
  }
}

function saveSessions(sessions: Session[]) {
  try {
    localStorage.setItem(sessionsKey(), JSON.stringify(sessions));
  } catch {
    /* best-effort */
  }
}

const newId = () => crypto.randomUUID();

const queues = new Map<string, Promise<void>>();
function serial(key: string, job: () => Promise<void>): Promise<void> {
  const next = (queues.get(key) ?? Promise.resolve()).then(job, job);
  queues.set(key, next);
  return next;
}

export const useAppStore = create<AppState>((set, get) => {
  const patch = (id: string, fn: (s: Session) => Partial<Session>) => {
    const sessions = get().sessions.map((s) => (s.id === id ? { ...s, ...fn(s) } : s));
    set({ sessions });
    saveSessions(sessions);
  };
  const append = (id: string, msgs: Omit<Message, 'id'>[]) =>
    patch(id, (s) => ({ messages: [...s.messages, ...msgs.map((m) => ({ ...m, id: newId() }))] }));
  const setRunning = (id: string, on: boolean) =>
    set((st) => ({ running: { ...st.running, [id]: on } }));

  // One global listener pair; events carry the session id. Dropped on hot reload
  // so the old store doesn't keep writing stale sessions to localStorage.
  const setDraft = (id: string, fn: (d: string) => string) =>
    set((st) => ({ drafts: { ...st.drafts, [id]: fn(st.drafts[id] ?? '') } }));
  const dropApproval = (sessionId: string, id: string) =>
    set((st) => ({ approvals: { ...st.approvals, [sessionId]: (st.approvals[sessionId] ?? []).filter((a) => a.id !== id) } }));

  const unlisten = onLocal({
    line: ({ session_id, line }) => {
      const runner = get().sessions.find((s) => s.id === session_id)?.runner;
      if (runner !== 'claude' && runner !== 'codex') return;
      const { resumeId, messages, results, delta } = parseLine(runner, line);
      if (resumeId && resumeId !== get().sessions.find((s) => s.id === session_id)?.resumeId)
        patch(session_id, () => ({ resumeId }));
      if (delta) setDraft(session_id, (d) => d + delta);
      // The finished text replaces what streamed in for it.
      if (messages.some((m) => m.role === 'assistant')) setDraft(session_id, () => '');
      if (messages.length) append(session_id, messages);
      if (results?.length)
        patch(session_id, (s) => ({
          messages: s.messages.map((m) => {
            const r = m.callId ? results.find((x) => x.callId === m.callId) : undefined;
            return r ? { ...m, result: r.text } : m;
          }),
        }));
    },
    approval: (a) =>
      set((st) => ({
        approvals: { ...st.approvals, [a.session_id]: [...(st.approvals[a.session_id] ?? []).filter((x) => x.id !== a.id), a] },
      })),
    resolved: ({ session_id, id }) => dropApproval(session_id, id),
    turnEnd: ({ session_id, error, interrupted }) => {
      // Text that streamed but never landed (e.g. Stop mid-reply) is kept.
      const draft = get().drafts[session_id]?.trim();
      if (draft) append(session_id, [{ role: 'assistant', text: draft }]);
      setDraft(session_id, () => '');
      set((st) => ({ approvals: { ...st.approvals, [session_id]: [] } }));
      setRunning(session_id, false);
      if (error) append(session_id, [{ role: 'error', text: error }]);
      else if (interrupted) append(session_id, [{ role: 'note', text: 'Stopped' }]);
    },
  }).catch(() => undefined); // not running inside Tauri (plain `vite`)
  import.meta.hot?.dispose(() => void unlisten.then((off) => off?.()));

  // Sign back in from the keychain on start (and after switching environment):
  // a refresh token outlives restarts, so the user isn't asked to log in again.
  const restore = async () => {
    set({ auth: { status: 'authenticating', user: null, error: null } });
    const saved = await restoreSession();
    if (!saved || !(await freshAccessToken())) {
      set({ auth: { status: 'unauthenticated', user: null, error: null } });
      return;
    }
    set({ auth: { status: 'authenticated', user: { email: saved.email }, error: null } });
    void get().connect();
  };
  onSessionExpired(() =>
    set({
      auth: { status: 'unauthenticated', user: null, error: 'Your session expired — sign in again.' },
      connection: 'offline',
    }),
  );
  queueMicrotask(() => void restore());
  setWorkspaceRef(activeWorkspace);
  void loadCodexModels().then((codexModels) => set({ codexModels }));

  return {
    settings: { theme: 'light' },
    toggleTheme: () =>
      set((s) => ({ settings: { theme: s.settings.theme === 'dark' ? 'light' : 'dark' } })),

    env: currentEnv(),
    setEnv: (id) => {
      if (get().auth.status === 'authenticating') return;
      saveEnv(id);
      dropSession();
      activeWorkspace = savedWorkspace();
      setWorkspaceRef(activeWorkspace);
      set({
        workspaces: [],
        workspaceId: activeWorkspace,
        env: currentEnv(),
        auth: { status: 'unauthenticated', user: null, error: null },
        connection: 'offline',
        mcps: [],
        skills: [],
        agents: [],
        sessions: loadSessions(),
        selectedId: null,
      });
      // Each environment keeps its own sign-in; pick it up if there is one.
      void restore();
    },

    // Starts as "authenticating" while the keychain is checked (see restore()).
    auth: { status: 'authenticating', user: null, error: null },

    login: async () => {
      set({ auth: { status: 'authenticating', user: null, error: null } });
      try {
        const verifier = randomToken();
        const challenge = await challengeFromVerifier(verifier);
        const state = randomToken();

        const { invoke } = await import('@tauri-apps/api/core');
        const { openUrl } = await import('@tauri-apps/plugin-opener');
        const { fetch } = await import('@tauri-apps/plugin-http');

        const as = await discover();
        const clientId = await ensureClient(as);

        // Start the loopback listener, then send the browser to sign in.
        const redirectPromise = invoke<string>('await_oauth_redirect', { port: LOOPBACK_PORT });
        await openUrl(buildAuthorizeUrl({ endpoint: as.authorization_endpoint, clientId, challenge, state }));

        let query: string;
        try {
          query = await redirectPromise;
        } catch (e) {
          // No redirect back usually means the auth server rejected the client
          // (e.g. it was reset); register a fresh one on the next attempt.
          forgetClient();
          throw e;
        }
        const params = new URLSearchParams(query);
        if (params.get('error'))
          throw new Error(params.get('error_description') || params.get('error')!);
        if (params.get('state') !== state) throw new Error('state mismatch (possible CSRF)');
        const code = params.get('code');
        if (!code) throw new Error('no authorization code returned');

        const body = new URLSearchParams({
          grant_type: 'authorization_code',
          code,
          redirect_uri: config.redirectUri,
          client_id: clientId,
          code_verifier: verifier,
        });
        const tokenRes = await fetch(as.token_endpoint, {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: body.toString(),
        });
        if (!tokenRes.ok) {
          if (tokenRes.status === 401) forgetClient();
          throw new Error(`token exchange failed (${tokenRes.status})`);
        }
        const tok = (await tokenRes.json()) as {
          access_token: string;
          refresh_token?: string;
          id_token?: string;
          expires_in?: number;
        };

        let email = 'signed in';
        try {
          if (!as.userinfo_endpoint) throw new Error('no userinfo endpoint');
          const ui = await fetch(as.userinfo_endpoint, {
            headers: { Authorization: `Bearer ${tok.access_token}` },
          });
          if (ui.ok) {
            const claims = (await ui.json()) as { email?: string; sub?: string };
            email = claims.email || claims.sub || email;
          }
        } catch {
          /* best-effort */
        }

        await setSession({
          accessToken: tok.access_token,
          refreshToken: tok.refresh_token,
          idToken: tok.id_token,
          expiresAt: Date.now() + (tok.expires_in ?? 3600) * 1000,
          clientId,
          tokenEndpoint: as.token_endpoint,
          email,
        });
        set({ auth: { status: 'authenticated', user: { email }, error: null } });
        void get().connect();
      } catch (e) {
        set({
          auth: {
            status: 'unauthenticated',
            user: null,
            error: e instanceof Error ? e.message : String(e),
          },
        });
      }
    },

    logout: () => {
      void clearSession();
      set({
        auth: { status: 'unauthenticated', user: null, error: null },
        connection: 'offline',
      });
    },

    devComplete: () => {
      set({ auth: { status: 'authenticated', user: { email: 'you@agentarea.dev' }, error: null } });
      void get().connect();
    },

    connection: 'offline',
    mcps: [],
    skills: [],
    agents: [],

    connect: async () => {
      set({ connection: 'connecting' });
      try {
        // Pick the workspace first: every other call is scoped by it.
        const workspaces = await listWorkspaces();
        const keep = workspaces.find((w) => w.id === activeWorkspace) ?? workspaces[0] ?? null;
        if ((keep?.id ?? null) !== activeWorkspace) {
          await get().setWorkspace(keep?.id ?? '');
        } else {
          setWorkspaceRef(activeWorkspace);
        }
        set({ workspaces, workspaceId: keep?.id ?? null });
        const [mcps, skills, agents] = await Promise.all([listMcpInstances(), listSkills(), listAgents()]);
        set({ connection: 'connected', mcps, skills, agents });
        void get().loadInbox();
      } catch {
        set({ connection: 'offline' });
      }
    },

    workspaces: [],
    workspaceId: activeWorkspace,
    setWorkspace: async (id) => {
      activeWorkspace = id || null;
      setWorkspaceRef(activeWorkspace);
      try {
        if (activeWorkspace) localStorage.setItem(workspaceKey(), activeWorkspace);
      } catch {
        /* best-effort */
      }
      set({ workspaceId: activeWorkspace, sessions: loadSessions(), selectedId: null });
      if (get().connection !== 'connected') return;
      try {
        const [mcps, skills, agents] = await Promise.all([listMcpInstances(), listSkills(), listAgents()]);
        set({ mcps, skills, agents, cloudInbox: [] });
        void get().loadInbox();
      } catch {
        set({ connection: 'offline' });
      }
    },

    codexModels: [],

    cloudInbox: [],
    loadInbox: async () => {
      if (get().connection !== 'connected') return set({ cloudInbox: [] });
      try {
        set({ cloudInbox: await listInbox() });
      } catch {
        /* keep the last list; the next poll retries */
      }
    },
    resolveCloud: async (item, approved) => {
      await resolveEscalation(item, approved);
      set((st) => ({ cloudInbox: st.cloudInbox.filter((i) => i.taskId !== item.taskId) }));
    },
    openCloudTask: (item) => {
      const existing = get().sessions.find((s) => s.runner === 'cloud' && s.resumeId === item.taskId);
      if (existing) return set({ selectedId: existing.id });
      const session: Session = {
        id: newId(),
        runner: 'cloud',
        agent: { id: item.agentId, name: item.agentName ?? 'Agent' },
        title: item.description.slice(0, 60) || 'Cloud task',
        createdAt: Date.now(),
        clientId: null,
        mcpEndpointUrl: null,
        mcpIds: [],
        skillIds: [],
        messages: [{ id: newId(), role: 'user', text: item.description }],
        // Follow-ups go to this task as queue_message (see lib/cloud.ts).
        resumeId: item.taskId,
        cwd: null,
        attachments: [],
        model: null,
        effort: null,
        readCount: 1,
      };
      const sessions = [session, ...get().sessions];
      set({ sessions, selectedId: session.id });
      saveSessions(sessions);
    },
    markRead: (sessionId) => {
      const s = get().sessions.find((x) => x.id === sessionId);
      if (s && s.readCount !== s.messages.length) patch(sessionId, (x) => ({ readCount: x.messages.length }));
    },

    sessions: loadSessions(),
    selectedId: null,
    running: {},
    drafts: {},
    approvals: {},
    approve: async (sessionId, id, decision) => {
      try {
        await approveLocal(sessionId, id, decision);
      } catch (e) {
        append(sessionId, [{ role: 'error', text: String(e) }]);
      }
      dropApproval(sessionId, id);
    },
    stop: async (sessionId) => {
      try {
        await stopLocal(sessionId);
      } catch (e) {
        append(sessionId, [{ role: 'error', text: String(e) }]);
      }
    },
    select: (id) => set({ selectedId: id }),

    newSession: async () => {
      // An untouched thread is reused instead of piling up empty ones.
      const blank = get().sessions.find((s) => s.messages.length === 0 && !s.clientId);
      if (blank) return set({ selectedId: blank.id });
      const session: Session = {
        id: newId(),
        runner: get().sessions[0]?.runner === 'codex' ? 'codex' : 'claude',
        agent: null,
        title: 'New thread',
        createdAt: Date.now(),
        clientId: null,
        mcpEndpointUrl: null,
        mcpIds: [],
        skillIds: [],
        messages: [],
        resumeId: null,
        cwd: null,
        attachments: [],
        model: null,
        effort: null,
        readCount: 0,
      };
      const sessions = [session, ...get().sessions];
      set({ sessions, selectedId: session.id });
      saveSessions(sessions);
    },

    setRunner: (sessionId, runner, agent) =>
      patch(sessionId, (s) =>
        s.messages.some((m) => m.role === 'user') ? {} : { runner, agent: runner === 'cloud' ? (agent ?? null) : null },
      ),

    // Both CLIs accept a different model/effort when resuming, so these stay
    // changeable for the whole thread.
    setModel: (sessionId, model) => patch(sessionId, () => ({ model, effort: null })),
    setEffort: (sessionId, effort) => patch(sessionId, () => ({ effort })),

    pickFolder: async (sessionId, clear) => {
      if (clear) return get().setFolder(sessionId, null);
      const { open } = await import('@tauri-apps/plugin-dialog');
      const dir = await open({ directory: true, multiple: false, title: 'Folder for this thread' });
      if (typeof dir === 'string') get().setFolder(sessionId, dir);
    },

    setFolder: (sessionId, path) => {
      const session = get().sessions.find((s) => s.id === sessionId);
      // The CLIs file their sessions under the working folder, so it is fixed
      // once the thread has started.
      if (!session || session.messages.some((m) => m.role === 'user')) return;
      patch(sessionId, () => ({ cwd: path }));
      if (!path) return;
      const recentFolders = [path, ...get().recentFolders.filter((p) => p !== path)].slice(0, 6);
      set({ recentFolders });
      try {
        localStorage.setItem('aa.recentFolders', JSON.stringify(recentFolders));
      } catch {
        /* best-effort */
      }
    },

    recentFolders: (() => {
      try {
        return JSON.parse(localStorage.getItem('aa.recentFolders') ?? '[]') as string[];
      } catch {
        return [];
      }
    })(),

    attachFiles: async (sessionId, paths) => {
      const session = get().sessions.find((s) => s.id === sessionId);
      if (!session || !paths.length) return;
      try {
        let attached = paths;
        // With a picked folder the agent reads files where they are; without
        // one, they are copied into the thread's own folder so it has them.
        if (!session.cwd) {
          const { invoke } = await import('@tauri-apps/api/core');
          const dest = await invoke<string>('session_dir', { sessionId });
          attached = await invoke<string[]>('import_files', { paths, dest });
        }
        patch(sessionId, (s) => ({ attachments: [...new Set([...s.attachments, ...attached])] }));
      } catch (e) {
        append(sessionId, [{ role: 'error', text: String(e) }]);
      }
    },

    referenceFiles: (sessionId, paths) =>
      patch(sessionId, (s) => ({ attachments: [...new Set([...s.attachments, ...paths])] })),

    removeAttachment: (sessionId, path) =>
      patch(sessionId, (s) => ({ attachments: s.attachments.filter((p) => p !== path) })),

    deleteSession: async (id) => {
      const session = get().sessions.find((s) => s.id === id);
      const sessions = get().sessions.filter((s) => s.id !== id);
      set((st) => ({ sessions, selectedId: st.selectedId === id ? null : st.selectedId }));
      saveSessions(sessions);
      if (session && session.runner !== 'cloud') void closeLocal(id).catch(() => {});
      if (session?.clientId) await deleteClient(session.clientId).catch(() => {});
    },

    toggleMember: async (sessionId, kind, id) => {
      const session = get().sessions.find((s) => s.id === sessionId);
      if (!session || get().connection !== 'connected') return;
      const key = kind === 'mcp' ? 'mcpIds' : 'skillIds';
      const had = session[key].includes(id);
      const flip = (ids: string[], on: boolean) => (on ? [...ids.filter((x) => x !== id), id] : ids.filter((x) => x !== id));
      patch(sessionId, (s) => ({ [key]: flip(s[key], !had) }));
      const member = kind === 'mcp' ? 'mcp-instances' : 'skills';
      // Server calls for one session run one at a time and read the client id
      // fresh, so quick clicks can't create two clients or reorder add/remove.
      return serial(sessionId, async () => {
        try {
          let clientId = get().sessions.find((s) => s.id === sessionId)?.clientId ?? null;
          if (!clientId) {
            // The client is created on the first pick, so empty sessions leave no trace.
            const client = await createClient(`Desktop session ${sessionId.slice(0, 8)}`);
            if (!get().sessions.some((s) => s.id === sessionId)) {
              await deleteClient(client.id).catch(() => {});
              return;
            }
            clientId = client.id;
            patch(sessionId, () => ({ clientId: client.id, mcpEndpointUrl: client.mcpEndpointUrl }));
          }
          await (had ? removeClientMember : addClientMember)(clientId, member, id);
        } catch (e) {
          patch(sessionId, (s) => ({ [key]: flip(s[key], had) }));
          append(sessionId, [{ role: 'error', text: String(e) }]);
        }
      });
    },

    send: async (sessionId, text) => {
      const session = get().sessions.find((s) => s.id === sessionId);
      if (!session || get().running[sessionId]) return;
      const files = session.attachments;
      const prompt = files.length ? `${text}\n\nAttached files:\n${files.map((f) => `- ${f}`).join('\n')}` : text;
      append(sessionId, [{ role: 'user', text: prompt }]);
      if (files.length) patch(sessionId, () => ({ attachments: [] }));
      if (!session.messages.some((m) => m.role === 'user'))
        patch(sessionId, () => ({ title: text.slice(0, 60) }));
      setRunning(sessionId, true);
      try {
        if (session.runner === 'cloud') {
          if (!session.agent) throw new Error('Pick a cloud agent first');
          await runCloud({
            agentId: session.agent.id,
            taskId: session.resumeId,
            prompt,
            onTaskId: (taskId) => patch(sessionId, () => ({ resumeId: taskId })),
            onMessages: (msgs) => append(sessionId, msgs),
          });
          setRunning(sessionId, false);
        } else {
          // Local turns end on the `agent-turn-end` event.
          await runLocal({
            runner: session.runner as LocalRunner,
            sessionId,
            prompt,
            resumeId: session.resumeId,
            mcpUrl: session.mcpEndpointUrl,
            token: await freshAccessToken(),
            cwd: session.cwd,
            model: session.model,
            effort: session.effort,
            // Cloud data sources: the agent reads workspace files as this user.
            workspace: get().workspaceId,
            workspaceName: get().workspaces.find((w) => w.id === get().workspaceId)?.name ?? null,
            apiBase: config.apiBaseUrl,
            toolsKey: [...session.mcpIds, ...session.skillIds].sort().join(','),
          });
        }
      } catch (e) {
        setRunning(sessionId, false);
        append(sessionId, [{ role: 'error', text: String(e) }]);
      }
    },
  };
});

// Dev builds only: reach the store from devtools / UI tests.
if (import.meta.env.DEV) (globalThis as { __aaStore?: typeof useAppStore }).__aaStore = useAppStore;
