// Fixture normalization for tools/list: tools sorted by name, object keys sorted
// recursively, 2-space JSON, trailing newline. Array order inside schemas is kept.
function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.keys(value).sort().map((k) => [k, sortKeys(value[k])]));
  }
  return value;
}

export function normalizeToolsList(result) {
  const tools = [...result.tools].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  return `${JSON.stringify(sortKeys({ tools }), null, 2)}\n`;
}
