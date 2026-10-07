import { describe, expect, test } from '@jest/globals';
import { chunkMarkdown } from '../src/docs/chunker.js';

describe('chunkMarkdown', () => {
  test('chunks markdown by headings with metadata', () => {
    const chunks = chunkMarkdown({
      family: 'australia',
      path: 'platform/admin/example.md',
      markdown: '# Page Title\n\nIntro.\n\n## First\n\nBody one.\n\n## Second\n\nBody two.'
    });

    expect(chunks).toHaveLength(3);
    expect(chunks[0]).toMatchObject({
      family: 'australia',
      path: 'platform/admin/example.md',
      title: 'Page Title',
      heading: 'Page Title'
    });
    expect(chunks[1].heading).toBe('First');
    expect(chunks[2].body).toContain('Body two.');
  });

  test('keeps line ranges for citations', () => {
    const chunks = chunkMarkdown({
      family: 'latest',
      path: 'foo.md',
      markdown: '# Title\n\nLine 3\n\n## Details\n\nLine 7'
    });

    expect(chunks[0].startLine).toBe(1);
    expect(chunks[0].endLine).toBe(4);
    expect(chunks[1].startLine).toBe(5);
  });

  test('recognises ATX headings with the same rules as before', () => {
    const chunks = chunkMarkdown({
      family: 'australia',
      path: 'h.md',
      markdown: '####### Not a heading\n#NoSpace\n#   \n#\tTabbed Title  \n\nbody'
    });

    expect(chunks.map((chunk) => chunk.heading)).toEqual(['Tabbed Title', 'Tabbed Title']);
    expect(chunks[0].body).toContain('####### Not a heading');
    expect(chunks[1].startLine).toBe(4);
  });

  test('handles adversarial heading-like lines in linear time', () => {
    const hostile = Array.from({ length: 200 }, () => `#${' '.repeat(16_000)}`).join('\n');
    const started = performance.now();
    chunkMarkdown({ family: 'australia', path: 'x.md', markdown: hostile });
    expect(performance.now() - started).toBeLessThan(500);
  });
});
