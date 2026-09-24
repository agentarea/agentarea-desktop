/**
 * Host theme → MCP Apps style variables, ported from the web app. The desktop
 * tokens are plain CSS colors (hex today), not HSL triplets like the web app's,
 * so values pass through as-is and tints use `color-mix`.
 */
import type { McpUiStyles } from '@modelcontextprotocol/ext-apps';

export const MCP_UI_STYLE_TOKEN_NAMES = [
  '--background',
  '--foreground',
  '--card',
  '--muted',
  '--muted-foreground',
  '--primary',
  '--destructive',
  '--border',
  '--ring',
  '--status-success',
  '--chart-2',
  '--font-sans',
  '--font-mono',
] as const;

export type CssTokenValues = Readonly<Record<string, string>>;

const FALLBACK_SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";
const FALLBACK_MONO = "ui-monospace, 'SF Mono', Menlo, Consolas, monospace";

function token(tokens: CssTokenValues, name: string, fallback: string): string {
  return tokens[name]?.trim() || fallback;
}

function tint(color: string, percent: number): string {
  return `color-mix(in srgb, ${color} ${percent}%, transparent)`;
}

/**
 * Pure mapping from a snapshot of the host's CSS custom properties; the caller
 * re-reads the snapshot whenever the theme changes.
 */
export function mcpUiStyleVariablesFromTokens(tokens: CssTokenValues): McpUiStyles {
  const background = token(tokens, '--background', '#ffffff');
  const foreground = token(tokens, '--foreground', '#0d0d0d');
  const card = token(tokens, '--card', background);
  const muted = token(tokens, '--muted', '#f5f5f5');
  const mutedForeground = token(tokens, '--muted-foreground', '#6b6b6b');
  const primary = token(tokens, '--primary', '#0d0d0d');
  const destructive = token(tokens, '--destructive', '#d92d20');
  const border = token(tokens, '--border', '#ececec');
  const ring = token(tokens, '--ring', foreground);
  const success = token(tokens, '--status-success', '#15803d');
  const warning = token(tokens, '--chart-2', '#d97706');

  return {
    '--color-background-primary': background,
    '--color-background-secondary': card,
    '--color-background-tertiary': muted,
    '--color-background-inverse': foreground,
    '--color-background-ghost': 'transparent',
    '--color-background-info': tint(primary, 8),
    '--color-background-danger': tint(destructive, 8),
    '--color-background-success': tint(success, 8),
    '--color-background-warning': tint(warning, 8),
    '--color-background-disabled': muted,

    '--color-text-primary': foreground,
    '--color-text-secondary': mutedForeground,
    '--color-text-tertiary': mutedForeground,
    '--color-text-inverse': background,
    '--color-text-ghost': mutedForeground,
    '--color-text-info': primary,
    '--color-text-danger': destructive,
    '--color-text-success': success,
    '--color-text-warning': warning,
    '--color-text-disabled': mutedForeground,

    '--color-border-primary': border,
    '--color-border-secondary': border,
    '--color-border-tertiary': muted,
    '--color-border-inverse': foreground,
    '--color-border-ghost': 'transparent',
    '--color-border-info': tint(primary, 30),
    '--color-border-danger': tint(destructive, 30),
    '--color-border-success': tint(success, 30),
    '--color-border-warning': tint(warning, 30),
    '--color-border-disabled': muted,

    '--color-ring-primary': ring,
    '--color-ring-secondary': mutedForeground,
    '--color-ring-inverse': background,
    '--color-ring-info': primary,
    '--color-ring-danger': destructive,
    '--color-ring-success': success,
    '--color-ring-warning': warning,

    '--font-sans': token(tokens, '--font-sans', FALLBACK_SANS),
    '--font-mono': token(tokens, '--font-mono', FALLBACK_MONO),
    '--font-weight-normal': '400',
    '--font-weight-medium': '500',
    '--font-weight-semibold': '600',
    '--font-weight-bold': '700',

    '--font-text-xs-size': '0.75rem',
    '--font-text-sm-size': '0.875rem',
    '--font-text-md-size': '1rem',
    '--font-text-lg-size': '1.125rem',
    '--font-heading-xs-size': '0.75rem',
    '--font-heading-sm-size': '0.875rem',
    '--font-heading-md-size': '1rem',
    '--font-heading-lg-size': '1.125rem',
    '--font-heading-xl-size': '1.25rem',
    '--font-heading-2xl-size': '1.5rem',
    '--font-heading-3xl-size': '1.875rem',
    '--font-text-xs-line-height': '1.333',
    '--font-text-sm-line-height': '1.429',
    '--font-text-md-line-height': '1.5',
    '--font-text-lg-line-height': '1.556',
    '--font-heading-xs-line-height': '1.333',
    '--font-heading-sm-line-height': '1.429',
    '--font-heading-md-line-height': '1.5',
    '--font-heading-lg-line-height': '1.4',
    '--font-heading-xl-line-height': '1.4',
    '--font-heading-2xl-line-height': '1.333',
    '--font-heading-3xl-line-height': '1.2',

    '--border-radius-xs': '2px',
    '--border-radius-sm': '4px',
    '--border-radius-md': '6px',
    '--border-radius-lg': '8px',
    '--border-radius-xl': '12px',
    '--border-radius-full': '9999px',
    '--border-width-regular': '1px',
    '--shadow-hairline': '0 1px 2px 0 rgb(0 0 0 / 0.04)',
    '--shadow-sm': '0 2px 10px rgb(0 0 0 / 0.04)',
    '--shadow-md': '0 4px 6px -1px rgb(0 0 0 / 0.1), 0 2px 4px -2px rgb(0 0 0 / 0.1)',
    '--shadow-lg': '0 10px 15px -3px rgb(0 0 0 / 0.1), 0 4px 6px -4px rgb(0 0 0 / 0.1)',
  };
}
