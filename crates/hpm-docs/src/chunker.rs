//! Heading-based markdown chunker (port of `src/docs/chunker.js`).

use crate::text::{is_js_whitespace, js_trim};

/// One citation-sized slice of a document. Line numbers are 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub family: String,
    pub path: String,
    pub title: String,
    pub heading: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
}

/// ATX heading: 1-6 `#`, then whitespace, then non-blank text. Linear scan.
fn heading_text(line: &str) -> Option<&str> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.chars().next().is_some_and(is_js_whitespace) {
        return None;
    }
    let text = js_trim(rest);
    (!text.is_empty()).then_some(text)
}

/// `markdown.split(/\r?\n/)`: a `\r` is removed only when it precedes a `\n`.
fn split_lines(markdown: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = markdown.split('\n').collect();
    let last = lines.len() - 1;
    for line in &mut lines[..last] {
        *line = line.strip_suffix('\r').unwrap_or(line);
    }
    lines
}

struct Open<'a> {
    heading: &'a str,
    start_line: u32,
    lines: Vec<&'a str>,
}

/// Splits `markdown` at ATX headings. Text before the first heading is attributed to the
/// document title; blank chunks are dropped.
#[must_use]
pub fn chunk_markdown(family: &str, path: &str, markdown: &str) -> Vec<Chunk> {
    let lines = split_lines(markdown);
    let title = lines
        .iter()
        .find_map(|line| heading_text(line))
        .unwrap_or(path);
    let mut chunks = Vec::new();
    let mut current: Option<Open<'_>> = None;

    let flush = |current: &Option<Open<'_>>, end_line: u32, chunks: &mut Vec<Chunk>| {
        let Some(open) = current else { return };
        let body = js_trim(&open.lines.join("\n")).to_owned();
        if !body.is_empty() {
            chunks.push(Chunk {
                family: family.to_owned(),
                path: path.to_owned(),
                title: title.to_owned(),
                heading: open.heading.to_owned(),
                start_line: open.start_line,
                end_line,
                body,
            });
        }
    };

    for (index, line) in lines.iter().enumerate() {
        let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        if let Some(heading) = heading_text(line) {
            flush(&current, line_number - 1, &mut chunks);
            current = Some(Open {
                heading,
                start_line: line_number,
                lines: vec![line],
            });
            continue;
        }
        current
            .get_or_insert_with(|| Open {
                heading: title,
                start_line: line_number,
                lines: Vec::new(),
            })
            .lines
            .push(line);
    }

    flush(
        &current,
        u32::try_from(lines.len()).unwrap_or(u32::MAX),
        &mut chunks,
    );
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Ported from tests/docs-chunker.test.js.

    #[test]
    fn chunks_markdown_by_headings_with_metadata() {
        let chunks = chunk_markdown(
            "australia",
            "platform/admin/example.md",
            "# Page Title\n\nIntro.\n\n## First\n\nBody one.\n\n## Second\n\nBody two.",
        );

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].family, "australia");
        assert_eq!(chunks[0].path, "platform/admin/example.md");
        assert_eq!(chunks[0].title, "Page Title");
        assert_eq!(chunks[0].heading, "Page Title");
        assert_eq!(chunks[1].heading, "First");
        assert!(chunks[2].body.contains("Body two."));
    }

    #[test]
    fn keeps_line_ranges_for_citations() {
        let chunks = chunk_markdown(
            "latest",
            "foo.md",
            "# Title\n\nLine 3\n\n## Details\n\nLine 7",
        );

        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 4);
        assert_eq!(chunks[1].start_line, 5);
    }

    #[test]
    fn recognises_atx_headings_with_the_same_rules_as_before() {
        let chunks = chunk_markdown(
            "australia",
            "h.md",
            "####### Not a heading\n#NoSpace\n#   \n#\tTabbed Title  \n\nbody",
        );

        let headings: Vec<&str> = chunks.iter().map(|chunk| chunk.heading.as_str()).collect();
        assert_eq!(headings, ["Tabbed Title", "Tabbed Title"]);
        assert!(chunks[0].body.contains("####### Not a heading"));
        assert_eq!(chunks[1].start_line, 4);
    }

    #[test]
    fn handles_adversarial_heading_like_lines_in_linear_time() {
        let hostile = vec![format!("#{}", " ".repeat(16_000)); 200].join("\n");
        let started = Instant::now();
        let _ = chunk_markdown("australia", "x.md", &hostile);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    // Additional edge cases for the JS `/\r?\n/` split and untitled documents.

    #[test]
    fn crlf_is_one_separator_and_lone_cr_is_kept() {
        let chunks = chunk_markdown("f", "p.md", "# T\r\nbody\rstill body\r\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].body, "# T\nbody\rstill body");
    }

    #[test]
    fn preamble_before_first_heading_uses_the_title_as_heading() {
        let chunks = chunk_markdown("f", "p.md", "intro\n# Real\nbody");
        assert_eq!(chunks[0].heading, "Real");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
        assert_eq!(chunks[1].heading, "Real");
    }

    #[test]
    fn documents_without_headings_fall_back_to_the_path_as_title() {
        let chunks = chunk_markdown("f", "dir/p.md", "just text");
        assert_eq!(chunks[0].title, "dir/p.md");
        assert_eq!(chunks[0].heading, "dir/p.md");
    }

    #[test]
    fn blank_documents_yield_no_chunks() {
        assert!(chunk_markdown("f", "p.md", "").is_empty());
        assert!(chunk_markdown("f", "p.md", "\n\n  \n").is_empty());
    }
}
