/**
 * How an MCP App reaches its server's tools, independent of where the server
 * runs: cloud apps go through the AgentArea API, apps on this device through
 * the local stdio runtime. Both hand McpAppFrame a plain MCP CallToolResult.
 */
import type { CallToolResult } from '@modelcontextprotocol/client';
import type { McpAppToolCall } from '@/lib/api';

export type CallAppTool = (call: McpAppToolCall) => Promise<CallToolResult>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function contentBlock(value: unknown): CallToolResult['content'][number] | null {
  if (!isRecord(value) || typeof value.type !== 'string') return null;

  if (value.type === 'text' && typeof value.text === 'string') {
    return { ...value, type: 'text', text: value.text };
  }
  if ((value.type === 'image' || value.type === 'audio') && typeof value.data === 'string' && typeof value.mimeType === 'string') {
    return { ...value, type: value.type, data: value.data, mimeType: value.mimeType };
  }
  if (value.type === 'resource_link' && typeof value.uri === 'string' && typeof value.name === 'string') {
    return { ...value, type: 'resource_link', uri: value.uri, name: value.name };
  }
  if (value.type === 'resource' && isRecord(value.resource)) {
    const resource = value.resource;
    if (typeof resource.uri !== 'string') return null;
    if (typeof resource.text === 'string') {
      return { ...value, type: 'resource', resource: { ...resource, uri: resource.uri, text: resource.text } };
    }
    if (typeof resource.blob === 'string') {
      return { ...value, type: 'resource', resource: { ...resource, uri: resource.uri, blob: resource.blob } };
    }
  }
  return null;
}

/** A loosely typed tool result (API response or raw MCP) → MCP CallToolResult. */
export function toCallToolResult(raw: { content?: unknown; structuredContent?: unknown; isError?: unknown }): CallToolResult {
  const content = Array.isArray(raw.content)
    ? raw.content.flatMap((block) => {
        const normalized = contentBlock(block);
        return normalized ? [normalized] : [];
      })
    : [];
  return {
    content,
    structuredContent: isRecord(raw.structuredContent) ? raw.structuredContent : undefined,
    isError: raw.isError === true,
  };
}
