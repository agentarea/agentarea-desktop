/** Last path segment for display; falls back to the whole string for a bare name. */
export function basename(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}
