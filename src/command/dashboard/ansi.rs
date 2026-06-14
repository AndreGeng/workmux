//! ANSI and tmux escape sequence handling utilities.

use ansi_to_tui::IntoText;
use ratatui::text::Line;

/// Strip terminal escape/control sequences from a string.
///
/// tmux captures can contain more than SGR color codes. Full-screen TUIs may emit
/// OSC/DCS/APC sequences for titles, hyperlinks, clipboard, or terminal graphics;
/// those must not be written back into the dashboard as text.
pub fn strip_ansi_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => strip_escape_sequence(&mut chars),
            '\u{009b}' => strip_csi(&mut chars),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                strip_string_sequence(&mut chars)
            }
            '\n' | '\t' => result.push(c),
            c if c.is_control() => {}
            _ => result.push(c),
        }
    }
    result
}

/// Keep display colors while removing terminal controls that cannot be safely
/// rendered inside the dashboard preview.
pub fn sanitize_ansi_for_preview(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => sanitize_escape_sequence(&mut chars, &mut result),
            '\u{009b}' => sanitize_csi(&mut chars, &mut result),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                strip_string_sequence(&mut chars)
            }
            '\n' | '\t' => result.push(c),
            c if c.is_control() => {}
            _ => result.push(c),
        }
    }
    result
}

fn sanitize_escape_sequence<I>(chars: &mut std::iter::Peekable<I>, result: &mut String)
where
    I: Iterator<Item = char>,
{
    match chars.next() {
        Some('[') => sanitize_csi(chars, result),
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => strip_string_sequence(chars),
        Some('(') | Some(')') | Some('*') | Some('+') | Some('-') | Some('.') | Some('/')
        | Some('%') | Some('#') => {
            let _ = chars.next();
        }
        Some(_) | None => {}
    }
}

fn sanitize_csi<I>(chars: &mut std::iter::Peekable<I>, result: &mut String)
where
    I: Iterator<Item = char>,
{
    let mut sequence = String::new();
    for c in chars.by_ref() {
        sequence.push(c);
        if ('\u{0040}'..='\u{007e}').contains(&c) {
            if c == 'm' {
                let params = sequence.strip_suffix('m').unwrap_or(&sequence);
                if let Some(sgr) = sanitize_sgr_params(params) {
                    result.push_str("\x1b[");
                    result.push_str(&sgr);
                    result.push('m');
                }
            }
            break;
        }
    }
}

fn sanitize_sgr_params(params: &str) -> Option<String> {
    if params.is_empty() {
        return Some(String::new());
    }

    let parts: Vec<&str> = params.split(';').collect();
    let mut kept = Vec::with_capacity(parts.len());
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        match part.parse::<u16>() {
            Ok(7) => i += 1, // inverse video turns foreground colors into backgrounds
            Ok(38) => {
                let color_len = sgr_color_param_len(&parts[i + 1..]).unwrap_or(0);
                kept.extend_from_slice(&parts[i..=(i + color_len).min(parts.len() - 1)]);
                i += 1 + color_len;
            }
            Ok(48) => {
                i += 1 + sgr_color_param_len(&parts[i + 1..]).unwrap_or(0);
            }
            Ok(40..=49) | Ok(100..=107) => i += 1,
            _ if part.starts_with("48:") => i += 1,
            _ if part.starts_with("38:") => {
                kept.push(part);
                i += 1;
            }
            _ => {
                kept.push(part);
                i += 1;
            }
        }
    }

    if kept.is_empty() {
        None
    } else {
        Some(kept.join(";"))
    }
}

fn sgr_color_param_len(rest: &[&str]) -> Option<usize> {
    match rest.first().copied() {
        Some("5") => Some(rest.len().min(2)),
        Some("2") => Some(rest.len().min(4)),
        Some(_) => Some(1),
        None => None,
    }
}

fn strip_escape_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.next() {
        Some('[') => strip_csi(chars),
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => strip_string_sequence(chars),
        Some('(') | Some(')') | Some('*') | Some('+') | Some('-') | Some('.') | Some('/')
        | Some('%') | Some('#') => {
            let _ = chars.next();
        }
        Some(_) | None => {}
    }
}

fn strip_csi<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    for c in chars.by_ref() {
        if ('\u{0040}'..='\u{007e}').contains(&c) {
            break;
        }
    }
}

fn strip_string_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(c) = chars.next() {
        match c {
            '\x07' | '\u{009c}' => break,
            '\x1b' if chars.peek() == Some(&'\\') => {
                let _ = chars.next();
                break;
            }
            _ => {}
        }
    }
}

/// Parse ANSI-escaped content into a vector of Lines for efficient rendering.
/// This is cached to avoid re-parsing on every frame.
pub fn parse_ansi_to_lines(content: &str) -> Vec<Line<'static>> {
    content
        .into_text()
        .map(|text| text.lines)
        .unwrap_or_else(|_| {
            // Fallback: split by newlines and create raw lines
            content.lines().map(|s| Line::raw(s.to_string())).collect()
        })
}

// Re-export from shared module for convenience within the dashboard.
pub use crate::tmux_style::parse_tmux_styles;

#[cfg(test)]
mod tests {
    use super::{sanitize_ansi_for_preview, strip_ansi_escapes};

    #[test]
    fn strips_sgr_color_sequences() {
        assert_eq!(strip_ansi_escapes("a\x1b[31mb\x1b[0mc"), "abc");
    }

    #[test]
    fn strips_osc_sequences() {
        assert_eq!(
            strip_ansi_escapes("before\x1b]8;;https://example.com\x07link\x1b]8;;\x07 after"),
            "beforelink after"
        );
    }

    #[test]
    fn strips_dcs_and_terminal_graphics_sequences() {
        assert_eq!(
            strip_ansi_escapes("top\x1b_Ga=T,f=100;AAAA\x1b\\bottom"),
            "topbottom"
        );
        assert_eq!(
            strip_ansi_escapes("top\x1bPtmux;\x1b\x1b[31mhidden\x1b\\bottom"),
            "topbottom"
        );
    }

    #[test]
    fn strips_raw_control_characters_but_keeps_lines_and_tabs() {
        assert_eq!(strip_ansi_escapes("a\x07b\rc\n\td"), "abc\n\td");
    }

    #[test]
    fn preview_sanitizer_preserves_sgr_color_sequences() {
        assert_eq!(
            sanitize_ansi_for_preview("a\x1b[38;2;255;106;193mb\x1b[0mc"),
            "a\x1b[38;2;255;106;193mb\x1b[0mc"
        );
    }

    #[test]
    fn preview_sanitizer_strips_background_sgr_sequences() {
        assert_eq!(
            sanitize_ansi_for_preview("a\x1b[48;2;10;20;30mb\x1b[0mc"),
            "ab\x1b[0mc"
        );
        assert_eq!(
            sanitize_ansi_for_preview("a\x1b[48;5;240mb\x1b[41mc\x1b[104md"),
            "abcd"
        );
    }

    #[test]
    fn preview_sanitizer_keeps_foreground_and_drops_background_in_combined_sgr() {
        assert_eq!(
            sanitize_ansi_for_preview("\x1b[1;38;2;255;106;193;48;2;10;10;10mtext"),
            "\x1b[1;38;2;255;106;193mtext"
        );
    }

    #[test]
    fn preview_sanitizer_strips_inverse_video() {
        assert_eq!(
            sanitize_ansi_for_preview("a\x1b[7mb\x1b[27mc"),
            "ab\x1b[27mc"
        );
    }

    #[test]
    fn preview_sanitizer_strips_cursor_and_screen_controls() {
        assert_eq!(
            sanitize_ansi_for_preview("a\x1b[2Jb\x1b[?1049hc\x1b[10;20Hd"),
            "abcd"
        );
    }

    #[test]
    fn preview_sanitizer_strips_graphics_but_preserves_following_color() {
        assert_eq!(
            sanitize_ansi_for_preview("top\x1b_Ga=T,f=100;AAAA\x1b\\\x1b[31mred\x1b[0m"),
            "top\x1b[31mred\x1b[0m"
        );
    }
}
