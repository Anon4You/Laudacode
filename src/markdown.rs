use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const CODE_FG: Color = Color::LightYellow;
const CODE_BG: Color = Color::Rgb(35, 35, 50);
const RULE: Color = Color::Rgb(95, 95, 120);

/// Render assistant markdown into styled transcript lines.
///
/// Supports fenced code blocks, headers, bullets, numbered lists,
/// blockquotes, horizontal rules, **bold**, *italic*, `` `code` `` and
/// [links](url) — bright palette throughout.
pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let w = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;

    for raw in text.split('\n') {
        let line = raw.trim_end();
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(rule_line(w));
            continue;
        }
        if in_code {
            for seg in hard_split(line, w.saturating_sub(3)) {
                out.push(Line::from(Span::styled(
                    format!("  {seg}"),
                    Style::default().fg(CODE_FG).bg(CODE_BG),
                )));
            }
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::from(String::new()));
            continue;
        }

        if trimmed == "---" || trimmed == "***" {
            out.push(rule_line(w));
            continue;
        }

        // Headers → light blue, bold.
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= 4 && trimmed[hashes..].starts_with(' ') {
            let body = trimmed[hashes..].trim();
            out.push(Line::from(String::new()));
            out.extend(wrap_styled(
                &[Span::styled(
                    body.to_string(),
                    Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
                )],
                w,
            ));
            continue;
        }

        // Bullets → light purple dot.
        if let Some(body) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            let mut spans = vec![Span::styled("• ".to_string(), Style::default().fg(Color::LightMagenta))];
            spans.extend(parse_inline(body));
            out.extend(wrap_styled(&spans, w));
            continue;
        }

        // Numbered lists.
        let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            let mut spans = vec![Span::styled(
                format!("{}. ", &trimmed[..digits]),
                Style::default().fg(Color::LightMagenta),
            )];
            spans.extend(parse_inline(&trimmed[digits + 2..]));
            out.extend(wrap_styled(&spans, w));
            continue;
        }

        // Blockquotes → italic light blue.
        if let Some(body) = trimmed.strip_prefix("> ") {
            out.extend(wrap_styled(
                &[Span::styled(
                    format!("▌ {body}"),
                    Style::default().fg(Color::LightBlue).add_modifier(Modifier::ITALIC),
                )],
                w,
            ));
            continue;
        }

        out.extend(wrap_styled(&parse_inline(line), w));
    }
    out
}

fn rule_line(w: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {}", "─".repeat(w.saturating_sub(2).min(58))),
        Style::default().fg(RULE),
    ))
}

/// Parse inline markdown (**bold**, *italic*, `` `code` ``, [link](url)).
pub fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let (mut bold, mut italic, mut code) = (false, false, false);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        if !code && rest.starts_with("**") {
            flush(&mut spans, &mut buf, bold, italic, code);
            bold = !bold;
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            flush(&mut spans, &mut buf, bold, italic, code);
            code = !code;
            i += 1;
            continue;
        }
        if !code && !bold && chars[i] == '*' {
            flush(&mut spans, &mut buf, bold, italic, code);
            italic = !italic;
            i += 1;
            continue;
        }
        if !code && chars[i] == '[' {
            if let Some(link_end) = parse_link(&rest) {
                flush(&mut spans, &mut buf, bold, italic, code);
                let (label, url, consumed) = link_end;
                spans.push(Span::styled(
                    label,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(format!(" ({url})"), Style::default().fg(Color::DarkGray)));
                i += consumed;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut spans, &mut buf, bold, italic, code);
    spans
}

/// Try to parse `[label](url)` at the start of `rest`.
/// Returns `(label, url, chars_consumed)`.
fn parse_link(rest: &str) -> Option<(String, String, usize)> {
    let close = rest.find("](")?;
    if close == 1 {
        return None; // empty label
    }
    let url_end = rest[close + 2..].find(')')?;
    let label = rest[1..close].to_string();
    let url = rest[close + 2..close + 2 + url_end].to_string();
    if label.contains('[') || url.is_empty() {
        return None;
    }
    Some((label, url, close + 2 + url_end + 1))
}

fn flush(spans: &mut Vec<Span<'static>>, buf: &mut String, bold: bool, italic: bool, code: bool) {
    if buf.is_empty() {
        return;
    }
    let mut style = Style::default();
    if code {
        style = style.fg(CODE_FG);
    } else {
        style = style.fg(Color::White);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
    }
    spans.push(Span::styled(std::mem::take(buf), style));
}

/// Word-wrap styled spans into rows of at most `width` display columns.
pub fn wrap_styled(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut cur_w = 0usize;

    for span in spans {
        for word in split_keep_spaces(&span.content) {
            let ww = UnicodeWidthStr::width(word.as_str());
            if cur_w > 0 && cur_w + ww > width && word != " " {
                rows.push(Vec::new());
                cur_w = 0;
                let word = word.trim_start().to_string();
                let ww = UnicodeWidthStr::width(word.as_str());
                rows.last_mut().unwrap().push(Span::styled(word, span.style));
                cur_w += ww;
            } else {
                rows.last_mut().unwrap().push(Span::styled(word, span.style));
                cur_w += ww;
            }
        }
    }
    rows.into_iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            Line::from(
                r.into_iter()
                    .map(|s| Span::styled(s.content, s.style))
                    .collect::<Vec<Span<'static>>>(),
            )
        })
        .collect()
}

/// Split into word tokens, keeping spaces attached to the preceding word so
/// reassembly preserves spacing.
fn split_keep_spaces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if ch == ' ' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Character-based hard split for code blocks (no word wrapping).
fn hard_split(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return vec![s.to_string()];
    }
    chars
        .chunks(width)
        .map(|c| c.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect::<Vec<_>>().join(""))
            .collect()
    }

    #[test]
    fn headers_are_light_blue_and_bold() {
        let md = "# Title\nbody";
        let lines = render_markdown(md, 40);
        let texts = plain(&lines);
        assert!(texts.iter().any(|t| t == "Title"));
        let title = lines.iter().find(|l| plain(std::slice::from_ref(l)) == ["Title"]).unwrap();
        assert_eq!(title.spans[0].style.fg, Some(Color::LightBlue));
        assert!(title.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_blocks_get_separator_rules_and_padding() {
        let md = "```rust\nlet x = 1;\n```";
        let lines = render_markdown(md, 40);
        let texts = plain(&lines);
        assert_eq!(texts.len(), 3); // rule, code, rule
        assert!(texts[1].contains("let x = 1;"));
        assert!(texts[1].starts_with("  "));
    }

    #[test]
    fn inline_code_is_light_yellow() {
        let spans = parse_inline("run `cargo build` now");
        let code = spans.iter().find(|s| s.content.contains("cargo build")).unwrap();
        assert_eq!(code.style.fg, Some(CODE_FG));
    }

    #[test]
    fn bold_spans_are_bold_white() {
        let spans = parse_inline("this is **important** stuff");
        let b = spans.iter().find(|s| s.content == "important").unwrap();
        assert!(b.style.add_modifier.contains(Modifier::BOLD));
        assert!(!b.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn links_render_label_plus_dim_url() {
        let spans = parse_inline("see [docs](https://x.y)");
        let texts: Vec<String> = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(texts.iter().any(|t| t == "docs"));
        assert!(texts.iter().any(|t| t.contains("https://x.y")));
    }

    #[test]
    fn bullets_become_dots() {
        let lines = render_markdown("- first item\n- second", 40);
        let texts = plain(&lines);
        assert!(texts[0].starts_with("• first item"));
        assert!(texts[1].starts_with("• second"));
    }

    #[test]
    fn styled_wrap_never_exceeds_width() {
        let spans = parse_inline(&"word ".repeat(60));
        let rows = wrap_styled(&spans, 20);
        for row in &rows {
            let total: usize = row.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
            assert!(total <= 20, "row width {total}");
        }
        assert!(rows.len() >= 5);
    }

    #[test]
    fn long_code_lines_hard_split() {
        let md = "```\nabcdefghij\n```";
        let lines = render_markdown(md, 6);
        let texts = plain(&lines);
        assert!(texts.len() >= 4); // rule + 2 wrapped + rule
    }
}
