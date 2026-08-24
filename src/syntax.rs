//! Lightweight syntax highlighting — no regex, no treesitter.
//!
//! A hand-rolled tokenizer good enough for transcript code blocks and diff
//! views: keywords, strings, comments (incl. multi-line), numbers, types,
//! function calls, macros/attributes. Language is detected from a fence tag
//! or a file path; unknown languages fall back to plain styling.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

// Token colors come from the active theme (see src/theme.rs).
pub fn c_kw() -> Color { crate::theme::get().kw }
pub fn c_string() -> Color { crate::theme::get().string }
pub fn c_comment() -> Color { crate::theme::get().comment }
pub fn c_number() -> Color { crate::theme::get().num }
pub fn c_type() -> Color { crate::theme::get().ty }
pub fn c_func() -> Color { crate::theme::get().func }
pub fn c_macro() -> Color { crate::theme::get().mac }
pub fn c_op() -> Color { crate::theme::get().op }

/// Multi-line tokenizer carry-over (C-family block comments).
#[derive(Default, Clone, Debug)]
pub struct SynState {
    pub in_block_comment: bool,
}

/// Map a markdown fence tag (`rust`) or file path (`src/main.rs`) to the
/// canonical internal language key. Empty string = no highlighting.
pub fn lang_of(tag_or_path: &str) -> &'static str {
    let t = tag_or_path.trim().to_lowercase();
    // A dot means we're looking at a filename/path — use its extension;
    // otherwise treat the tag itself as the language name.
    let key = if t.contains('.') { t.rsplit('.').next().unwrap_or(&t) } else { t.as_str() };
    match key {
        "rust" | "rs" => "rust",
        "python" | "py" => "python",
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "mjs" | "cjs" => "js",
        "go" | "golang" => "go",
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hh" => "cpp",
        "java" | "cs" | "kt" | "swift" => "java",
        "sh" | "bash" | "zsh" | "shell" => "sh",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        _ => "",
    }
}

fn is_hash_comment(lang: &str) -> bool {
    matches!(lang, "python" | "sh" | "toml" | "yaml")
}

fn has_block_comments(lang: &str) -> bool {
    matches!(lang, "rust" | "cpp" | "java" | "js" | "go")
}

const KEYWORDS: &[&str] = &[
    // rust
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while", "abstract", "final", "override", "sealed", "union",
    // python
    "and", "assert", "class", "def", "del", "elif", "except", "finally", "from", "global",
    "import", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "try", "with",
    "yield", "case", "print",
    // js/ts
    "function", "var", "delete", "do", "export", "extends", "get", "set", "instanceof",
    "interface", "namespace", "new", "of", "package", "private", "protected", "public",
    "readonly", "switch", "throws", "typeof", "void",
    // go
    "chan", "default", "defer", "fallthrough", "func", "goto", "range", "select", "map",
    // c/cpp/java
    "auto", "char", "double", "explicit", "float", "friend", "inline", "int", "long",
    "mutable", "namespace", "noexcept", "operator", "short", "signed", "sizeof", "template",
    "typedef", "typename", "unsigned", "using", "virtual", "volatile", "synchronized",
    "extends", "implements", "boolean", "byte", "throws", "native", "transient", "then",
    "elif", "fi", "done", "esac", "local", "export", "echo", "cd", "source", "alias",
];

const LITERALS: &[&str] = &["true", "false", "null", "nil", "none", "None", "True", "False"];

const TYPES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "string",
    "u8", "u16", "u32", "u64", "u128", "usize", "vec", "String", "Option", "Result", "Box",
    "HashMap", "HashSet", "BTreeMap", "Rc", "Arc", "int", "float", "list", "dict", "tuple",
    "set", "bytes", "object", "any", "unknown", "never", "number", "boolean",
];

/// Tokenize ONE line into styled spans. `state` carries block-comment
/// context across successive lines of the same snippet. Unknown languages
/// return a single plain span in `base`.
pub fn highlight_line(line: &str, lang: &str, state: &mut SynState, base: Style) -> Vec<Span<'static>> {
    if lang.is_empty() {
        return vec![Span::styled(line.to_string(), base)];
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let push_plain = |out: &mut Vec<Span<'static>>, plain: &mut String| {
        if !plain.is_empty() {
            out.push(Span::styled(std::mem::take(plain), base));
        }
    };
    let styled = |out: &mut Vec<Span<'static>>, s: String, c: Color, it: bool, base: Style| {
        if !s.is_empty() {
            let st = base.fg(c).add_modifier(if it { Modifier::ITALIC } else { Modifier::empty() });
            out.push(Span::styled(s, st));
        }
    };

    let mut i = 0usize;
    while i < chars.len() {
        // Ongoing block comment?
        if state.in_block_comment {
            let rest: String = chars[i..].iter().collect();
            match rest.find("*/") {
                Some(end) => {
                    let seg: String = chars[i..=i + end + 1].iter().collect();
                    styled(&mut out, seg, c_comment(), true, base);
                    i += end + 2;
                    state.in_block_comment = false;
                }
                None => {
                    let seg: String = chars[i..].iter().collect();
                    styled(&mut out, seg, c_comment(), true, base);
                    return out;
                }
            }
            continue;
        }

        let rest: String = chars[i..].iter().collect();

        // Comments.
        if rest.starts_with("//") {
            let seg: String = chars[i..].iter().collect();
            styled(&mut out, seg, c_comment(), true, base);
            break;
        }
        if has_block_comments(lang) && rest.starts_with("/*") {
            state.in_block_comment = true;
            continue;
        }
        if is_hash_comment(lang) && chars[i] == '#' {
            let seg: String = chars[i..].iter().collect();
            styled(&mut out, seg, c_comment(), true, base);
            break;
        }
        // C preprocessor directives.
        if lang == "cpp" && chars[i] == '#' && plain.trim().is_empty() && out.is_empty() {
            let seg: String = chars[i..].iter().collect();
            styled(&mut out, seg, c_macro(), false, base);
            break;
        }
        // Rust attributes #[...].
        if lang == "rust" && chars[i] == '#' && chars.get(i + 1) == Some(&'[') {
            let depth_start = i;
            let mut depth = 0usize;
            let mut j = i;
            while j < chars.len() {
                if chars[j] == '[' { depth += 1; }
                if chars[j] == ']' {
                    depth -= 1;
                    if depth == 0 { break; }
                }
                j += 1;
            }
            let seg: String = chars[depth_start..=j.min(chars.len() - 1)].iter().collect();
            push_plain(&mut out, &mut plain);
            styled(&mut out, seg, c_macro(), false, base);
            i = j + 1;
            continue;
        }
        // Decorators / annotations.
        if matches!(lang, "python" | "js" | "java") && chars[i] == '@' {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.') {
                j += 1;
            }
            let seg: String = chars[i..j].iter().collect();
            push_plain(&mut out, &mut plain);
            styled(&mut out, seg, c_macro(), false, base);
            i = j;
            continue;
        }
        // Strings (with escapes). Unterminated runs to EOL.
        if matches!(chars[i], '"' | '\'' | '`') && lang != "json" || (lang == "json" && chars[i] == '"') {
            let quote = chars[i];
            let mut j = i + 1;
            let mut escaped = false;
            while j < chars.len() {
                if escaped { escaped = false; }
                else if chars[j] == '\\' { escaped = true; }
                else if chars[j] == quote { break; }
                j += 1;
            }
            let seg: String = chars[i..j.min(chars.len())].iter().collect();
            push_plain(&mut out, &mut plain);
            styled(&mut out, seg, c_string(), false, base);
            i = j + 1;
            continue;
        }
        // Numbers.
        if chars[i].is_ascii_digit()
            || (chars[i] == '.' && chars.get(i + 1).map(|c| c.is_ascii_digit()).unwrap_or(false))
        {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric()
                    || chars[j] == '_'
                    || chars[j] == '.'
                    || ((chars[j] == '+' || chars[j] == '-') && j > i && (chars[j - 1] == 'e' || chars[j - 1] == 'E')))
            {
                j += 1;
            }
            let seg: String = chars[i..j].iter().collect();
            push_plain(&mut out, &mut plain);
            styled(&mut out, seg, c_number(), false, base);
            i = j;
            continue;
        }
        // Identifiers / keywords.
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // Look ahead past spaces for '(' (call) or '!' (macro).
            let mut k = i;
            while k < chars.len() && chars[k] == ' ' {
                k += 1;
            }
            let color = if word == "self" || word == "this" {
                c_type()
            } else if LITERALS.contains(&word.as_str()) {
                c_number()
            } else if KEYWORDS.contains(&word.as_str()) {
                c_kw()
            } else if TYPES.contains(&word.as_str()) || word.chars().next().is_some_and(|c| c.is_uppercase()) {
                c_type()
            } else if chars.get(k) == Some(&'(') {
                c_func()
            } else if chars.get(i) == Some(&'!') {
                // Rust macro: include the '!'.
                i += 1;
                let seg: String = chars[start..i].iter().collect();
                push_plain(&mut out, &mut plain);
                styled(&mut out, seg, c_macro(), false, base);
                continue;
            } else {
                // Plain identifier — fold into the plain buffer.
                plain.push_str(&word);
                continue;
            };
            push_plain(&mut out, &mut plain);
            let it = color == c_comment();
            styled(&mut out, word, color, it, base);
            continue;
        }
        // Operators / punctuation — slightly brighter than plain text.
        if "+-*/%=<>!&|^~:".contains(chars[i]) {
            push_plain(&mut out, &mut plain);
            styled(&mut out, chars[i].to_string(), c_op(), false, base);
            i += 1;
            continue;
        }
        plain.push(chars[i]);
        i += 1;
    }
    push_plain(&mut out, &mut plain);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Style = Style::new();

    fn texts(spans: &[Span]) -> Vec<String> {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn rust_keywords_and_calls_are_colored() {
        let mut st = SynState::default();
        let spans = highlight_line("let total = compute(x);", "rust", &mut st, BASE);
        let find = |needle: &str| {
            spans.iter()
                .find(|s| s.content.split(&[' ', ':', '<']).any(|w| w == needle))
                .unwrap_or_else(|| panic!("no span containing word {needle:?} in {spans:?}"))
        };
        assert_eq!(find("let").style.fg, Some(c_kw()));
        assert_eq!(find("compute").style.fg, Some(c_func()));
        // Plain identifiers keep the base foreground.
        let plain_total = spans.iter().find(|s| s.content.contains("total")).unwrap();
        assert_eq!(plain_total.style.fg, BASE.fg);
    }

    #[test]
    fn strings_numbers_comments() {
        let mut st = SynState::default();
        let spans = highlight_line("s = \"hi\" # note", "python", &mut st, BASE);
        let str_span = spans.iter().find(|s| s.content.contains("hi"))
            .unwrap_or_else(|| panic!("no string span in {spans:?}"));
        assert_eq!(str_span.style.fg, Some(c_string()));
        let com = spans.iter().find(|s| s.content.contains("note"))
            .unwrap_or_else(|| panic!("no comment span in {spans:?}"));
        assert_eq!(com.style.fg, Some(c_comment()));
        let mut st = SynState::default();
        let spans = highlight_line("let n = 0xFF;", "rust", &mut st, BASE);
        assert_eq!(spans.iter().find(|s| s.content == "0xFF").unwrap().style.fg, Some(c_number()));
    }

    #[test]
    fn block_comments_carry_across_lines() {
        let mut st = SynState::default();
        let l1 = highlight_line("/* start", "rust", &mut st, BASE);
        assert!(st.in_block_comment);
        assert_eq!(l1.last().unwrap().style.fg, Some(c_comment()));
        let l2 = highlight_line("end */ let x;", "rust", &mut st, BASE);
        assert!(!st.in_block_comment);
        let kw = l2.iter().find(|s| s.content == "let").unwrap();
        assert_eq!(kw.style.fg, Some(c_kw()));
    }

    #[test]
    fn camel_case_reads_as_type_and_macros() {
        let mut st = SynState::default();
        let spans = highlight_line("let v: Vec<u8> = format!(\"x\");", "rust", &mut st, BASE);
        assert_eq!(spans.iter().find(|s| s.content == "Vec").unwrap().style.fg, Some(c_type()));
        assert!(spans.iter().any(|s| s.content == "format!" && s.style.fg == Some(c_macro())));
    }

    #[test]
    fn unknown_language_falls_back_to_base() {
        let mut st = SynState::default();
        let spans = highlight_line("<html>plain</html>", "", &mut st, BASE);
        assert_eq!(texts(&spans), vec!["<html>plain</html>".to_string()]);
        assert_eq!(spans[0].style, BASE);
    }

    #[test]
    fn lang_detection_from_paths_and_tags() {
        assert_eq!(lang_of("rust"), "rust");
        assert_eq!(lang_of("src/main.rs"), "rust");
        assert_eq!(lang_of("scripts/run.sh"), "sh");
        assert_eq!(lang_of("Cargo.toml"), "toml");
        assert_eq!(lang_of("weird.unknown"), "");
    }

    #[test]
    fn base_style_is_preserved_under_tokens() {
        let base = Style::default().fg(Color::Green).bg(Color::Rgb(10, 10, 10));
        let mut st = SynState::default();
        let spans = highlight_line("let x", "rust", &mut st, base);
        let kw = spans.iter().find(|s| s.content == "let").unwrap();
        assert_eq!(kw.style.bg, Some(Color::Rgb(10, 10, 10)), "background survives");
        assert_eq!(kw.style.fg, Some(c_kw()), "token color wins");
    }
}
