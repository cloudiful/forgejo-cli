//! Find the Markdown references of a pull request body.
//!
//! The body is parsed with `comrak`, so links and images inside code blocks,
//! inline code and raw HTML are not references. Only the destination of a
//! reference is located in the source, the rest of the body is never touched.

use std::ops::Range;

use comrak::nodes::NodeValue;

/// A link or image of the body with the range of its destination.
pub(super) struct BodyRef {
    /// The destination as it is written in the body, without angle brackets.
    pub dest: String,
    /// The byte range of the destination in the body, with angle brackets.
    pub dest_range: Range<usize>,
}

/// Find the destination of every link and image of the body.
pub(super) fn scan_body_refs(body: &str) -> Vec<BodyRef> {
    let arena = comrak::Arena::new();
    let options = comrak::Options::default();
    let root = comrak::parse_document(&arena, body, &options);
    let offsets = line_offsets(body);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let blocks = definition_blocks(root, &lines);
    let mut refs = Vec::new();
    for node in root.descendants() {
        {
            let data = node.data.borrow();
            match data.value {
                NodeValue::Link(_) | NodeValue::Image(_) => {}
                _ => continue,
            }
        }
        let sourcepos = node.data.borrow().sourcepos;
        let start = offset(&offsets, sourcepos.start.line, sourcepos.start.column);
        let last = offset(&offsets, sourcepos.end.line, sourcepos.end.column);
        // The end of a source position is the last character of the node.
        let end = last + body[last..].chars().next().map(char::len_utf8).unwrap_or(0);
        let text = &body[start..end];
        let Some(label_end) = label_end(text) else {
            continue;
        };
        if let Some((from, to, angled)) = inline_destination(text, label_end) {
            let dest = text[from..to].to_owned();
            let dest_range = if angled {
                (start + from - 1)..(start + to + 1)
            } else {
                (start + from)..(start + to)
            };
            refs.push(BodyRef { dest, dest_range });
            continue;
        }
        let Some(label) = reference_label(text, label_end) else {
            continue;
        };
        let Some((from, to, angled)) = definition_destination(&lines, &blocks, &label) else {
            continue;
        };
        let dest = body[from..to].to_owned();
        let dest_range = if angled {
            (from - 1)..(to + 1)
        } else {
            from..to
        };
        refs.push(BodyRef { dest, dest_range });
    }
    refs
}

/// Whether the destination points to something outside of the repository.
pub(super) fn is_remote_destination(dest: &str) -> bool {
    if dest.starts_with('#') || dest.starts_with("//") {
        return true;
    }
    match dest.split_once(':') {
        Some((scheme, _)) => {
            let mut chars = scheme.chars();
            match chars.next() {
                Some(first) if first.is_ascii_alphabetic() => {
                    scheme.len() > 1
                        && chars
                            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
                }
                _ => false,
            }
        }
        None => false,
    }
}

/// The spellings a destination can have.
///
/// A destination can be written with Markdown escapes or percent encoding, so
/// every spelling that could name the same file is tried.
pub(super) fn destination_spellings(dest: &str) -> Vec<String> {
    let mut spellings = vec![dest.to_owned()];
    let unescaped = unescape_backslash(dest);
    if unescaped != dest {
        spellings.push(unescaped.clone());
    }
    if let Some(decoded) = percent_decode(&unescaped)
        && !spellings.contains(&decoded)
    {
        spellings.push(decoded);
    }
    if let Some(decoded) = percent_decode(dest)
        && !spellings.contains(&decoded)
    {
        spellings.push(decoded);
    }
    spellings
}

fn unescape_backslash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(escaped) = chars.next()
        {
            out.push(escaped);
            continue;
        }
        out.push(c);
    }
    out
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

fn line_offsets(body: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in body.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn offset(offsets: &[usize], line: usize, column: usize) -> usize {
    offsets[line - 1] + column - 1
}

/// The index of the `]` that closes the label of a link or image.
fn label_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = usize::from(bytes.first() == Some(&b'!'));
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    index += 1;
    let mut depth = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 2;
                continue;
            }
            b'`' => {
                index += code_span_end(bytes, index);
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The index after the code span that starts at `index`.
fn code_span_end(bytes: &[u8], index: usize) -> usize {
    let mut ticks = 0;
    while bytes.get(index + ticks) == Some(&b'`') {
        ticks += 1;
    }
    let mut scan = index + ticks;
    while scan < bytes.len() {
        if bytes[scan] == b'`' {
            let mut run = 0;
            while bytes.get(scan + run) == Some(&b'`') {
                run += 1;
            }
            if run == ticks {
                return scan + run;
            }
            scan += run;
            continue;
        }
        scan += 1;
    }
    bytes.len()
}

/// The range of the destination of an inline link or image.
fn inline_destination(text: &str, label_end: usize) -> Option<(usize, usize, bool)> {
    let bytes = text.as_bytes();
    let mut index = label_end + 1;
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    index += 1;
    while matches!(bytes.get(index), Some(b' ') | Some(b'\t') | Some(b'\n')) {
        index += 1;
    }
    if bytes.get(index) == Some(&b'<') {
        let start = index + 1;
        let mut scan = start;
        while scan < bytes.len() {
            match bytes[scan] {
                b'\\' => {
                    scan += 2;
                    continue;
                }
                b'>' => return Some((start, scan, true)),
                _ => {}
            }
            scan += 1;
        }
        return None;
    }
    let start = index;
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 2;
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b' ' | b'\t' | b'\n' => break,
            _ => {}
        }
        index += 1;
    }
    if index == start {
        return None;
    }
    Some((start, index, false))
}

/// The label of a reference-style link, which is the label of its definition.
fn reference_label(text: &str, label_end: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let open = usize::from(bytes.first() == Some(&b'!'));
    let link_text = &text[open + 1..label_end];
    if bytes.get(label_end + 1) == Some(&b'[') {
        let rest = &text[label_end + 2..];
        let close = rest.find(']')?;
        if close == 0 {
            return Some(link_text.to_owned());
        }
        return Some(rest[..close].to_owned());
    }
    Some(link_text.to_owned())
}

/// The blocks of the body that can hold a link reference definition.
///
/// A definition is only a definition at the start of a paragraph, and the
/// first definition of a label in the document wins. Definitions are removed
/// from the parsed body, so a paragraph that only holds definitions has no
/// node of its own; those regions are found from the lines that no other block
/// covers.
struct DefinitionBlocks {
    /// The first and last line of every block, in document order.
    blocks: Vec<(usize, usize)>,
    /// The lines of code blocks and raw HTML blocks.
    opaque: Vec<usize>,
}

impl DefinitionBlocks {
    /// The block that holds the line.
    fn block_of(&self, line: usize) -> Option<(usize, usize)> {
        if self.opaque.contains(&line) {
            return None;
        }
        self.blocks
            .iter()
            .copied()
            .find(|(start, end)| *start <= line && line <= *end)
    }

    /// Whether the line can hold a definition of the block.
    ///
    /// Whether the line really is a definition is decided by the parser in
    /// `is_winning_definition`, which needs the start of the block.
    fn definition_block(&self, line: usize) -> Option<usize> {
        self.block_of(line).map(|(start, _)| start)
    }
}

fn definition_blocks<'a>(root: &'a comrak::nodes::AstNode<'a>, lines: &[&str]) -> DefinitionBlocks {
    let mut text_blocks = Vec::new();
    let mut opaque_blocks = Vec::new();
    for node in root.descendants() {
        let data = node.data.borrow();
        let sourcepos = data.sourcepos;
        let range = (sourcepos.start.line, sourcepos.end.line);
        match &data.value {
            NodeValue::CodeBlock(_) | NodeValue::HtmlBlock(_) => opaque_blocks.push(range),
            NodeValue::Paragraph | NodeValue::Heading(_) => text_blocks.push(range),
            _ => {}
        }
    }

    let covered = |line: usize| {
        text_blocks
            .iter()
            .chain(opaque_blocks.iter())
            .any(|(start, end)| *start <= line && line <= *end)
    };

    // A run of lines that no node covers holds definitions, which the parser
    // removed from the body.
    let mut blocks = text_blocks.clone();
    let mut index = 0;
    while index < lines.len() {
        let line = index + 1;
        let blank = lines[index].trim().is_empty();
        if blank || covered(line) {
            index += 1;
            continue;
        }
        let start = line;
        let mut end = line;
        index += 1;
        while index < lines.len() {
            let next = index + 1;
            if lines[index].trim().is_empty() || covered(next) {
                break;
            }
            end = next;
            index += 1;
        }
        blocks.push((start, end));
    }
    blocks.sort_unstable();

    DefinitionBlocks {
        blocks,
        opaque: opaque_blocks.iter().map(|(start, _)| *start).collect(),
    }
}

/// Whether the line looks like a link reference definition.
fn is_definition_line(text: &str) -> bool {
    let definition = text.trim_start_matches(' ');
    if !definition.starts_with('[') {
        return false;
    }
    let Some(close) = definition.find(']') else {
        return false;
    };
    definition[close + 1..].starts_with(':')
}

/// The range of the destination of a link reference definition.
fn definition_destination(
    lines: &[&str],
    blocks: &DefinitionBlocks,
    label: &str,
) -> Option<(usize, usize, bool)> {
    let wanted = normalize_label(label);
    if wanted.is_empty() || label.contains(['[', ']']) {
        // A label that cannot be written as a reference is left alone.
        return None;
    }
    let mut line_start = 0;
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let text = line.trim_end_matches(['\n', '\r']);
        let indent = text.len() - text.trim_start_matches(' ').len();
        let definition = text.trim_start_matches(' ');
        let candidate = if indent <= 3 && is_definition_line(text) {
            definition.find(']').and_then(|close| {
                if normalize_label(&definition[1..close]) != wanted {
                    return None;
                }
                let rest = definition[close + 1..].strip_prefix(':')?;
                let (from, to, angled) = definition_destination_range(rest)?;
                Some((rest, from, to, angled))
            })
        } else {
            None
        };
        if let Some((rest, from, to, angled)) = candidate
            && let Some(block_start) = blocks.definition_block(number)
            && is_winning_definition(lines, block_start, number, label, &rest[from..to])
        {
            let start = line_start + text.len() - rest.len();
            return Some((start + from, start + to, angled));
        }
        line_start += line.len();
    }
    None
}

/// Whether the line is the definition of the label that the parser uses.
///
/// A line is only the definition when no earlier text of the body already
/// defines the label, and when the parser registers the label with this
/// destination once the line is part of its block.
fn is_winning_definition(
    lines: &[&str],
    block_start: usize,
    line: usize,
    label: &str,
    destination: &str,
) -> bool {
    let before_block: String = lines[..block_start - 1].concat();
    let before_line: String = lines[block_start - 1..line - 1].concat();
    for probe in [
        probe_label(&before_block, label),
        probe_label(&before_line, label),
    ] {
        match probe {
            LabelProbe::Defined(_) => return false,
            // A probe that cannot prove the label is undefined must not let the
            // candidate through, so the file is appended instead.
            LabelProbe::Unknown => return false,
            LabelProbe::Absent => {}
        }
    }
    let with_line: String = lines[block_start - 1..line].concat();
    matches!(
        probe_destination(&with_line, label, destination),
        LabelProbe::Defined(_)
    )
}

/// The most references the parser resolves against before it stops counting.
const REFERENCE_BUDGET: usize = 100_000;

/// The padding that raises a probe to the largest budget the parser allows.
const PROBE_PADDING: usize = REFERENCE_BUDGET;

/// What a probe of a text says about a label.
enum LabelProbe {
    /// The text defines the label with this destination.
    Defined(String),
    /// The text does not define the label.
    Absent,
    /// The probe could not tell, so the label is treated as possibly defined.
    Unknown,
}

/// Whether the text defines the label at all.
fn probe_label(text: &str, label: &str) -> LabelProbe {
    if text.trim().is_empty() {
        return LabelProbe::Absent;
    }
    probe(text, label)
}

/// Whether the text defines the label with the destination.
fn probe_destination(text: &str, label: &str, destination: &str) -> LabelProbe {
    match probe(text, label) {
        LabelProbe::Defined(url) if destination_spellings(destination).contains(&url) => {
            LabelProbe::Defined(url)
        }
        LabelProbe::Defined(_) => LabelProbe::Absent,
        other => other,
    }
}

/// What a probe reference to the label resolves to in the text.
///
/// The parser only resolves references until they have used up the length of
/// the document, so a padded paragraph in front of the probe raises that budget
/// to its cap. The resolver also reports how much of that budget the text
/// itself used: when its references could have used up the rest, the probe
/// answers [`LabelProbe::Unknown`] instead of claiming the label is undefined,
/// so a candidate is never accepted on a truncated probe.
fn probe(text: &str, label: &str) -> LabelProbe {
    let probe = format!(
        "{}\n\n{text}\n\n[probe][{label}]",
        "x".repeat(PROBE_PADDING)
    );
    let arena = comrak::Arena::new();
    let options = comrak::Options::default();
    let root = comrak::parse_document(&arena, &probe, &options);
    let probe_line = probe.lines().count();
    let mut consumed = 0usize;
    let mut longest = longest_line(text).saturating_mul(2);
    let mut resolved = None;
    for node in root.descendants() {
        let data = node.data.borrow();
        let link = match &data.value {
            NodeValue::Link(link) | NodeValue::Image(link) => link,
            _ => continue,
        };
        let size = link.url.len() + link.title.len();
        if data.sourcepos.start.line == probe_line {
            resolved = Some(link.url.clone());
            continue;
        }
        consumed += size;
        longest = longest.max(size);
    }
    match resolved {
        Some(url) => LabelProbe::Defined(url),
        None if consumed.saturating_add(longest) > REFERENCE_BUDGET => LabelProbe::Unknown,
        None => LabelProbe::Absent,
    }
}

/// The length of the longest line of the text.
fn longest_line(text: &str) -> usize {
    text.lines().map(str::len).max().unwrap_or(0)
}

/// The range of the destination inside a definition, relative to it.
fn definition_destination_range(text: &str) -> Option<(usize, usize, bool)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while matches!(bytes.get(index), Some(b' ') | Some(b'\t')) {
        index += 1;
    }
    if bytes.get(index) == Some(&b'<') {
        let start = index + 1;
        let mut scan = start;
        while scan < bytes.len() {
            match bytes[scan] {
                b'\\' => {
                    scan += 2;
                    continue;
                }
                b'>' => return Some((start, scan, true)),
                _ => {}
            }
            scan += 1;
        }
        return None;
    }
    let start = index;
    while index < bytes.len() && !matches!(bytes[index], b' ' | b'\t') {
        index += 1;
    }
    if index == start {
        return None;
    }
    Some((start, index, false))
}

/// The label of a link reference definition, normalized like CommonMark does.
fn normalize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut space = false;
    for c in label.trim().chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        for lower in c.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(body: &str) -> Vec<(String, String)> {
        scan_body_refs(body)
            .into_iter()
            .map(|r| (r.dest.clone(), body[r.dest_range].to_owned()))
            .collect()
    }

    #[test]
    fn inline_destinations_are_found() {
        let body = "![alt text](./shot.png)\n\n[label](<./doc file.pdf> \"the title\")\n";
        assert_eq!(
            refs(body),
            vec![
                ("./shot.png".to_owned(), "./shot.png".to_owned()),
                ("./doc file.pdf".to_owned(), "<./doc file.pdf>".to_owned()),
            ]
        );
    }

    #[test]
    fn code_and_prose_are_not_references() {
        let body = "\
```md
![code](./shot.png)
```

    ![indented](./shot.png)

inline `![code](./shot.png)` and prose ./shot.png
";
        assert!(refs(body).is_empty());
    }

    #[test]
    fn reference_definitions_are_found() {
        let body = "![alt][ref]\n\n[ref]: ./target.png \"ref title\"\n";
        assert_eq!(
            refs(body),
            vec![("./target.png".to_owned(), "./target.png".to_owned())]
        );
    }

    #[test]
    fn a_definition_inside_a_fence_is_not_a_definition() {
        let body = "\
```
[ref]: ./fenced.png
```

![alt][ref]

[ref]: ./real.png
";
        assert_eq!(
            refs(body),
            vec![("./real.png".to_owned(), "./real.png".to_owned())]
        );
    }

    #[test]
    fn a_definition_inside_an_html_block_is_not_a_definition() {
        let body = "\
<div>
[ref]: ./html.png
</div>

![alt][ref]

[ref]: ./real.png
";
        assert_eq!(
            refs(body),
            vec![("./real.png".to_owned(), "./real.png".to_owned())]
        );
    }

    #[test]
    fn a_definition_in_the_middle_of_a_paragraph_is_not_a_definition() {
        let body = "Some text\n[ref]: ./prose.png\n\n![alt][ref]\n";
        assert!(refs(body).is_empty());
    }

    #[test]
    fn an_invalid_definition_prefix_is_not_a_definition() {
        let body = "\
[foo]:
[bar]: ./fake.png

see [bar]

[bar]: ./real.png
";
        // The shortcut link `[bar]` and the definition both resolve to the real
        // definition on the last line, never to the invalid prefix line.
        assert_eq!(
            refs(body),
            vec![
                ("./real.png".to_owned(), "./real.png".to_owned()),
                ("./real.png".to_owned(), "./real.png".to_owned()),
            ]
        );
    }

    #[test]
    fn adjacent_definitions_are_definitions() {
        let body = "[a]: ./a.png\n[b]: ./b.png\n\n![x][b]\n";
        assert_eq!(
            refs(body),
            vec![("./b.png".to_owned(), "./b.png".to_owned())]
        );
    }

    #[test]
    fn the_first_definition_of_a_label_wins() {
        let body = "![x][bar]\n\n[bar]: ./first.png\n\n[bar]: ./second.png\n";
        assert_eq!(
            refs(body),
            vec![("./first.png".to_owned(), "./first.png".to_owned())]
        );
    }

    #[test]
    fn a_definition_label_is_matched_case_insensitively() {
        let body = "![x][BAR]\n\n[bar]: ./shot.png\n";
        assert_eq!(
            refs(body),
            vec![("./shot.png".to_owned(), "./shot.png".to_owned())]
        );
    }

    #[test]
    fn container_and_split_definitions_are_left_alone() {
        // The parser resolves these definitions, but the destination is not on
        // a definition line of its own, so it is not rewritten: the file is
        // appended instead and the local reference stays.
        for body in [
            "- [shot]: ./shot.png\n\n![x][shot]\n",
            "> [shot]: ./shot.png\n\n![x][shot]\n",
            "[shot]:\n  ./shot.png\n\n![x][shot]\n",
        ] {
            assert!(refs(body).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn a_later_duplicate_is_not_a_definition() {
        // The list item on line 1 registers the label; line 3 is paragraph text
        // that only looks like a definition.
        let body = "- [shot]: ./shot.png\ntext\n[shot]: ./shot.png\n\n![x][shot]\n";
        assert!(refs(body).is_empty(), "{body:?}");
    }

    #[test]
    fn a_split_definition_wins_over_a_later_duplicate() {
        let body = "[shot]:\n  ./shot.png\n\ntext\n[shot]: ./shot.png\n";
        assert!(refs(body).is_empty(), "{body:?}");
    }

    #[test]
    fn a_definition_on_its_own_line_inside_a_list_item_is_rewritten() {
        let body = "-\n  [shot]: ./shot.png\n\n![x][shot]\n";
        assert_eq!(
            refs(body),
            vec![("./shot.png".to_owned(), "./shot.png".to_owned())]
        );
    }

    #[test]
    fn a_definition_after_a_block_quote_marker_is_left_alone() {
        let body = ">\n> [shot]: ./shot.png\n\n![x][shot]\n";
        assert!(refs(body).is_empty(), "{body:?}");
    }

    #[test]
    fn a_probe_that_cannot_prove_absence_rejects_the_candidate() {
        let long_url = format!("./{}", "a".repeat(60_000));
        let heavy = format!("[ref]: {long_url}\n\n![a][ref]\n\n![b][ref]\n");

        // The references of the heavy text could use up the parser budget, so
        // the probe cannot claim the label is undefined.
        assert!(matches!(probe_label(&heavy, "ref_b"), LabelProbe::Unknown));

        // A candidate behind that text is rejected instead of rewritten.
        let body = format!("{heavy}\n[ref]: ./real.png\n\n![c][ref]\n");
        let lines: Vec<&str> = body.split_inclusive('\n').collect();
        assert!(
            !is_winning_definition(&lines, 7, 7, "ref", "./real.png"),
            "a candidate behind an undecidable probe is not the winner"
        );

        // The same candidate is accepted when the probe can decide.
        let small = "[ref]: ./shot.png\n\n![a][ref]\n\n[ref]: ./real.png\n";
        let lines: Vec<&str> = small.split_inclusive('\n').collect();
        assert!(
            !is_winning_definition(&lines, 5, 5, "ref", "./real.png"),
            "the earlier definition wins"
        );
        let unique = "[ref]: ./shot.png\n\n![a][ref]\n\n[other]: ./real.png\n";
        let lines: Vec<&str> = unique.split_inclusive('\n').collect();
        assert!(
            is_winning_definition(&lines, 5, 5, "other", "./real.png"),
            "an unduplicated definition is accepted"
        );
    }

    #[test]
    fn a_duplicate_in_another_block_is_not_a_definition() {
        // The list item on line 1 wins; the bare duplicate in its own block is
        // not the definition the parser uses.
        let body = "- [shot]: ./shot.png\n\nsee [shot]\n\n[shot]: ./shot.png\n\n![x][shot]\n";
        assert!(refs(body).is_empty(), "{body:?}");
    }

    #[test]
    fn a_split_winner_in_another_block_wins() {
        let body = "[shot]:\n  ./shot.png\n\ntext\n\n[shot]: ./shot.png\n\n![x][shot]\n";
        assert!(refs(body).is_empty(), "{body:?}");
    }

    #[test]
    fn remote_and_anchor_destinations_are_recognized() {
        assert!(is_remote_destination("https://example.com/x.png"));
        assert!(is_remote_destination("mailto:someone@example.com"));
        assert!(is_remote_destination("//example.com/x.png"));
        assert!(is_remote_destination("#section"));
        assert!(!is_remote_destination("./shot.png"));
        assert!(!is_remote_destination("shot.png"));
    }

    #[test]
    fn destination_spellings_cover_escapes_and_encoding() {
        assert_eq!(
            destination_spellings("my%20file.png"),
            vec!["my%20file.png".to_owned(), "my file.png".to_owned()]
        );
        assert_eq!(
            destination_spellings("my\\ file.png"),
            vec!["my\\ file.png".to_owned(), "my file.png".to_owned()]
        );
    }
}
