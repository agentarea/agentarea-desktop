#!/usr/bin/env node
// A tiny MCP server over stdio (one JSON-RPC message per line), no
// dependencies. AgentArea Desktop starts it from this folder with
// AGENTAREA_PLUGIN_ID and AGENTAREA_DATA_DIR (a per-workspace folder for the
// plugin's data) and serves it at the plugin's gateway URL.
// `.mjs`: an ES module wherever the folder lives, whatever package.json is around.
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

const dataDir = process.env.AGENTAREA_DATA_DIR || path.join(import.meta.dirname, '.data');
const counterFile = path.join(dataDir, 'counter.json');

const tools = [
  {
    name: 'echo',
    description: 'Return the given text unchanged.',
    inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
  },
  {
    name: 'counter_increment',
    description: "Add to this workspace's counter (default 1) and return the new value. Persisted in the plugin's data folder.",
    inputSchema: { type: 'object', properties: { by: { type: 'integer', default: 1 } } },
  },
];

function readCount() {
  try {
    return Number(JSON.parse(fs.readFileSync(counterFile, 'utf8')).count) || 0;
  } catch {
    return 0;
  }
}

function writeCount(count) {
  fs.mkdirSync(dataDir, { recursive: true });
  const tmp = `${counterFile}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ count }) + '\n');
  fs.renameSync(tmp, counterFile);
}

const text = (t, isError = false) => ({ content: [{ type: 'text', text: String(t) }], isError });

function callTool(name, args) {
  switch (name) {
    case 'echo':
      if (typeof args.text !== 'string') return text('"text" must be a string', true);
      return text(args.text);
    case 'counter_increment': {
      const by = args.by === undefined ? 1 : args.by;
      if (!Number.isInteger(by)) return text('"by" must be an integer', true);
      const count = readCount() + by;
      writeCount(count);
      return text(count);
    }
    default:
      return null;
  }
}

function handle(msg) {
  const { id, method, params = {} } = msg;
  switch (method) {
    case 'initialize':
      return {
        protocolVersion: params.protocolVersion || '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'backend-demo', version: '0.1.0' },
      };
    case 'ping':
      return {};
    case 'tools/list':
      return { tools };
    case 'tools/call': {
      const result = callTool(params.name, params.arguments || {});
      if (!result) throw Object.assign(new Error(`unknown tool: ${params.name}`), { code: -32602 });
      return result;
    }
    default:
      throw Object.assign(new Error(`${method} is not supported`), { code: -32601 });
  }
}

const send = (m) => process.stdout.write(JSON.stringify(m) + '\n');

readline.createInterface({ input: process.stdin }).on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return send({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } });
  }
  if (msg.id === undefined || typeof msg.method !== 'string') return; // notification or response
  try {
    send({ jsonrpc: '2.0', id: msg.id, result: handle(msg) });
  } catch (e) {
    send({ jsonrpc: '2.0', id: msg.id, error: { code: e.code || -32603, message: e.message } });
  }
});
