const WHITESPACE = /\s/;

// ATX heading: 1-6 "#", then whitespace, then non-blank text. Linear scan.
function headingText(line) {
  let level = 0;
  while (level < line.length && line[level] === '#') level += 1;
  if (level < 1 || level > 6 || !WHITESPACE.test(line[level] ?? '')) return null;
  return line.slice(level).trim() || null;
}

function firstHeading(lines) {
  for (const line of lines) {
    const heading = headingText(line);
    if (heading) return heading;
  }
  return null;
}

export function chunkMarkdown({ family, path, markdown }) {
  const lines = markdown.split(/\r?\n/);
  const title = firstHeading(lines) || path;
  const chunks = [];
  let current = null;

  function flush(endLine) {
    if (!current) return;
    const body = current.lines.join('\n').trim();
    if (body) {
      chunks.push({
        family,
        path,
        title,
        heading: current.heading,
        startLine: current.startLine,
        endLine,
        body
      });
    }
  }

  lines.forEach((line, index) => {
    const lineNumber = index + 1;
    const heading = headingText(line);
    if (heading) {
      flush(lineNumber - 1);
      current = {
        heading,
        startLine: lineNumber,
        lines: [line]
      };
      return;
    }

    if (!current) {
      current = {
        heading: title,
        startLine: lineNumber,
        lines: []
      };
    }
    current.lines.push(line);
  });

  flush(lines.length);
  return chunks;
}
