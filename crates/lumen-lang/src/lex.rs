//! The lexer.
//!
//! Whitespace is insignificant, comments run to end of line, and there are no
//! semicolons — statements end at a newline. But an expression may wrap across
//! lines while brackets are open, so the lexer emits [`Tok::Newline`] and tracks
//! bracket depth, suppressing newlines inside brackets. Doing it here rather
//! than in the parser keeps "when does a statement end" in exactly one place.
//!
//! **Units are part of the literal, not decoration.** `90deg` is lexed as a
//! number carrying [`Unit::Deg`] and converted to radians at parse time. That is
//! cheap here and removes an entire category of "why is my rotation 57 times too
//! fast".

use alloc::string::String;
use alloc::vec::Vec;

use crate::diag::{Diagnostic, Span};

/// A literal's unit suffix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    /// Metres.
    M,
    /// Seconds.
    S,
    /// Milliseconds.
    Ms,
    /// Degrees. Converted to radians at parse time.
    Deg,
    /// Radians.
    Rad,
    /// Hertz.
    Hz,
    /// Percent. Converted to a 0..1 fraction at parse time.
    Percent,
}

impl Unit {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Unit> {
        Some(match s {
            "m" => Unit::M,
            "s" => Unit::S,
            "ms" => Unit::Ms,
            "deg" => Unit::Deg,
            "rad" => Unit::Rad,
            "hz" => Unit::Hz,
            "%" => Unit::Percent,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Unit::M => "m",
            Unit::S => "s",
            Unit::Ms => "ms",
            Unit::Deg => "deg",
            Unit::Rad => "rad",
            Unit::Hz => "hz",
            Unit::Percent => "%",
        }
    }
}

/// A token.
#[derive(Clone, PartialEq, Debug)]
pub enum Tok {
    Ident(String),
    /// A numeric literal and its unit suffix, if any.
    ///
    /// The value is kept as the raw decimal text so the parser can decide how to
    /// convert it — an `int` context and a `float` context round differently,
    /// and losing that here would be irreversible.
    Number {
        text: String,
        unit: Option<Unit>,
    },
    /// `#RRGGBB` or `#RRGGBBAA`.
    HexColor([u8; 4]),
    Str(String),
    /// End of a statement.
    Newline,
    // Punctuation and operators.
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    DotDot,
    Assign,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    Ne,
    AndAnd,
    OrOr,
    Bang,
    Eof,
}

impl Tok {
    /// A short name for use in diagnostics.
    pub fn describe(&self) -> &'static str {
        match self {
            Tok::Ident(_) => "an identifier",
            Tok::Number { .. } => "a number",
            Tok::HexColor(_) => "a colour",
            Tok::Str(_) => "a string",
            Tok::Newline => "end of line",
            Tok::LBrace => "`{`",
            Tok::RBrace => "`}`",
            Tok::LParen => "`(`",
            Tok::RParen => "`)`",
            Tok::LBracket => "`[`",
            Tok::RBracket => "`]`",
            Tok::Comma => "`,`",
            Tok::Colon => "`:`",
            Tok::Dot => "`.`",
            Tok::DotDot => "`..`",
            Tok::Assign => "`=`",
            Tok::Arrow => "`->`",
            Tok::Plus => "`+`",
            Tok::Minus => "`-`",
            Tok::Star => "`*`",
            Tok::Slash => "`/`",
            Tok::Percent => "`%`",
            Tok::Lt => "`<`",
            Tok::Le => "`<=`",
            Tok::Gt => "`>`",
            Tok::Ge => "`>=`",
            Tok::EqEq => "`==`",
            Tok::Ne => "`!=`",
            Tok::AndAnd => "`&&`",
            Tok::OrOr => "`||`",
            Tok::Bang => "`!`",
            Tok::Eof => "end of file",
        }
    }
}

/// A token with its position in the source.
#[derive(Clone, PartialEq, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// A comment, kept rather than thrown away.
///
/// The formatter has to put these back. Text is the canonical format and the
/// node editor is a view over it, so a round trip that silently deleted every
/// comment would take an author's explanation of *why* an effect works and
/// discard it - which is the one thing a diffable text format was supposed to
/// protect.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Comment {
    /// The text after `#`, trimmed of a single leading space.
    pub text: String,
    pub span: Span,
    /// True when the comment sits on a line of its own.
    ///
    /// A trailing comment belongs to the line it follows; a standalone one
    /// belongs to whatever comes next. Getting this wrong moves a comment away
    /// from the thing it explains, which is worse than losing it — a wrong
    /// explanation reads as true.
    pub own_line: bool,
    /// True when a blank line separated this comment from whatever preceded it.
    ///
    /// Needed to keep a file-level header apart from the comment on the
    /// declaration below it. Run together, the header reads as documenting that
    /// one declaration rather than the file.
    pub blank_before: bool,
}

/// Turn source text into tokens.
///
/// Returns as many tokens as it can even when something is wrong, so the parser
/// can report several problems in one run rather than one per compile.
pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let (tokens, _, diags) = lex_with_comments(src);
    (tokens, diags)
}

/// Lex, keeping the comments.
pub fn lex_with_comments(src: &str) -> (Vec<Token>, Vec<Comment>, Vec<Diagnostic>) {
    Lexer::new(src).run()
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    depth: u32,
    out: Vec<Token>,
    comments: Vec<Comment>,
    errs: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Lexer {
            src: text.as_bytes(),
            text,
            pos: 0,
            depth: 0,
            out: Vec::new(),
            comments: Vec::new(),
            errs: Vec::new(),
        }
    }

    fn peek(&self) -> u8 {
        *self.src.get(self.pos).unwrap_or(&0)
    }

    fn peek_at(&self, n: usize) -> u8 {
        *self.src.get(self.pos + n).unwrap_or(&0)
    }

    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.pos += 1;
        c
    }

    fn push(&mut self, tok: Tok, start: usize) {
        self.out.push(Token {
            tok,
            span: Span::new(start, self.pos),
        });
    }

    fn run(mut self) -> (Vec<Token>, Vec<Comment>, Vec<Diagnostic>) {
        while self.pos < self.src.len() {
            let start = self.pos;
            let c = self.peek();
            match c {
                b' ' | b'\t' | b'\r' => {
                    self.pos += 1;
                }
                b'\n' => {
                    self.pos += 1;
                    // An expression may wrap while brackets are open, so a
                    // newline inside them is not a statement terminator.
                    if self.depth == 0 {
                        // Collapse runs of blank lines: a parser that has to
                        // skip them everywhere gets them wrong somewhere.
                        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                            self.push(Tok::Newline, start);
                        }
                    }
                }
                b'#' => self.hash(start),
                b'"' => self.string(start),
                b'0'..=b'9' => self.number(start),
                c if is_ident_start(c) => self.ident(start),
                _ => self.punct(start),
            }
        }
        let end = self.src.len();
        if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
            self.out.push(Token {
                tok: Tok::Newline,
                span: Span::new(end, end),
            });
        }
        self.out.push(Token {
            tok: Tok::Eof,
            span: Span::new(end, end),
        });
        (self.out, self.comments, self.errs)
    }

    /// `#` starts either a comment or a hex colour. Distinguished by what
    /// follows: six or eight hex digits ending at a non-word character is a
    /// colour, anything else is a comment.
    fn hash(&mut self, start: usize) {
        let hex_len = (1..)
            .take_while(|&i| self.peek_at(i).is_ascii_hexdigit())
            .count();
        let terminated = !is_ident_continue(self.peek_at(hex_len + 1));
        if (hex_len == 6 || hex_len == 8) && terminated {
            self.pos += 1;
            let mut rgba = [0u8, 0, 0, 255];
            for slot in rgba.iter_mut().take(hex_len / 2) {
                let hi = hex_val(self.bump());
                let lo = hex_val(self.bump());
                *slot = hi * 16 + lo;
            }
            self.push(Tok::HexColor(rgba), start);
        } else {
            // Whether anything but whitespace precedes it on this line decides
            // where the comment belongs when the file is reformatted.
            let line_start = self.text[..start].rfind('\n').map_or(0, |i| i + 1);
            let own_line = self.text[line_start..start].trim().is_empty();
            // A blank line before it means the line above held nothing. Without
            // this a file header runs into the comment on the declaration below
            // and then reads as documenting only that declaration.
            let blank_before = own_line
                && line_start > 0
                && self.text[..line_start - 1]
                    .rfind('\n')
                    .map(|prev| self.text[prev + 1..line_start - 1].trim().is_empty())
                    .unwrap_or(false);
            self.pos += 1; // the `#`
            let from = self.pos;
            while self.pos < self.src.len() && self.peek() != b'\n' {
                self.pos += 1;
            }
            let text = self.text[from..self.pos].trim_end();
            let text = text.strip_prefix(' ').unwrap_or(text);
            self.comments.push(Comment {
                text: text.into(),
                span: Span::new(start, self.pos),
                own_line,
                blank_before,
            });
        }
    }

    fn string(&mut self, start: usize) {
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            if self.pos >= self.src.len() {
                self.errs.push(Diagnostic::error(
                    Span::new(start, self.pos),
                    "unterminated string",
                    "add a closing `\"`",
                ));
                break;
            }
            match self.bump() {
                b'"' => break,
                b'\\' => {
                    let e = self.bump();
                    s.push(match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'\\' => '\\',
                        b'"' => '"',
                        other => {
                            self.errs.push(Diagnostic::error(
                                Span::new(self.pos - 2, self.pos),
                                "unknown escape sequence",
                                "supported escapes are \\n, \\t, \\\\ and \\\"",
                            ));
                            other as char
                        }
                    });
                }
                b'\n' => {
                    self.errs.push(Diagnostic::error(
                        Span::new(start, self.pos),
                        "unterminated string",
                        "strings may not span lines",
                    ));
                    break;
                }
                c => {
                    // Re-decode the UTF-8 sequence this byte begins.
                    let len = utf8_len(c);
                    let from = self.pos - 1;
                    self.pos = from + len;
                    let slice = self.text.get(from..self.pos.min(self.text.len()));
                    match slice.and_then(|s| s.chars().next()) {
                        Some(ch) => s.push(ch),
                        None => s.push(c as char),
                    }
                }
            }
        }
        self.push(Tok::Str(s), start);
    }

    fn number(&mut self, start: usize) {
        while self.peek().is_ascii_digit() {
            self.pos += 1;
        }
        // A `.` is only a decimal point if a digit follows; `1..2` is a range.
        if self.peek() == b'.' && self.peek_at(1).is_ascii_digit() {
            self.pos += 1;
            while self.peek().is_ascii_digit() {
                self.pos += 1;
            }
        }
        let text = self.text[start..self.pos].into();

        // Unit suffix, if any.
        let unit_start = self.pos;
        if self.peek() == b'%' {
            self.pos += 1;
            self.push(
                Tok::Number {
                    text,
                    unit: Some(Unit::Percent),
                },
                start,
            );
            return;
        }
        while is_ident_continue(self.peek()) {
            self.pos += 1;
        }
        let unit = if unit_start == self.pos {
            None
        } else {
            let raw = &self.text[unit_start..self.pos];
            match Unit::from_str(raw) {
                Some(u) => Some(u),
                None => {
                    self.errs.push(Diagnostic::error(
                        Span::new(unit_start, self.pos),
                        "unknown unit",
                        "units are m, s, ms, deg, rad, hz and %",
                    ));
                    None
                }
            }
        };
        self.push(Tok::Number { text, unit }, start);
    }

    fn ident(&mut self, start: usize) {
        while is_ident_continue(self.peek()) {
            self.pos += 1;
        }
        self.push(Tok::Ident(self.text[start..self.pos].into()), start);
    }

    fn punct(&mut self, start: usize) {
        let c = self.bump();
        let two = self.peek();
        let tok = match (c, two) {
            (b'-', b'>') => {
                self.pos += 1;
                Tok::Arrow
            }
            (b'.', b'.') => {
                self.pos += 1;
                Tok::DotDot
            }
            (b'<', b'=') => {
                self.pos += 1;
                Tok::Le
            }
            (b'>', b'=') => {
                self.pos += 1;
                Tok::Ge
            }
            (b'=', b'=') => {
                self.pos += 1;
                Tok::EqEq
            }
            (b'!', b'=') => {
                self.pos += 1;
                Tok::Ne
            }
            (b'&', b'&') => {
                self.pos += 1;
                Tok::AndAnd
            }
            (b'|', b'|') => {
                self.pos += 1;
                Tok::OrOr
            }
            (b'{', _) => Tok::LBrace,
            (b'}', _) => Tok::RBrace,
            (b'(', _) => {
                self.depth += 1;
                Tok::LParen
            }
            (b')', _) => {
                self.depth = self.depth.saturating_sub(1);
                Tok::RParen
            }
            (b'[', _) => {
                self.depth += 1;
                Tok::LBracket
            }
            (b']', _) => {
                self.depth = self.depth.saturating_sub(1);
                Tok::RBracket
            }
            (b',', _) => Tok::Comma,
            (b':', _) => Tok::Colon,
            (b'.', _) => Tok::Dot,
            (b'=', _) => Tok::Assign,
            (b'+', _) => Tok::Plus,
            (b'-', _) => Tok::Minus,
            (b'*', _) => Tok::Star,
            (b'/', _) => Tok::Slash,
            (b'%', _) => Tok::Percent,
            (b'<', _) => Tok::Lt,
            (b'>', _) => Tok::Gt,
            (b'!', _) => Tok::Bang,
            _ => {
                self.errs.push(Diagnostic::error(
                    Span::new(start, self.pos),
                    "unexpected character",
                    "this character has no meaning in an effect file",
                ));
                return;
            }
        };
        self.push(tok, start);
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn toks(src: &str) -> Vec<Tok> {
        let (t, e) = lex(src);
        assert!(e.is_empty(), "unexpected diagnostics: {e:?}");
        t.into_iter().map(|t| t.tok).collect()
    }

    fn num(text: &str, unit: Option<Unit>) -> Tok {
        Tok::Number {
            text: text.into(),
            unit,
        }
    }

    #[test]
    fn lexes_an_empty_file() {
        assert_eq!(toks(""), vec![Tok::Eof]);
    }

    #[test]
    fn lexes_identifiers_and_keywords_alike() {
        // Keywords are not special to the lexer; the parser decides.
        assert_eq!(
            toks("effect foo _bar b2"),
            vec![
                Tok::Ident("effect".into()),
                Tok::Ident("foo".into()),
                Tok::Ident("_bar".into()),
                Tok::Ident("b2".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(
            toks("a # this is a comment\nb"),
            vec![
                Tok::Ident("a".into()),
                Tok::Newline,
                Tok::Ident("b".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn a_hash_is_a_colour_when_it_looks_like_one() {
        // The ambiguity worth getting right: `#` starts both comments and hex
        // colours, and the only difference is what follows.
        assert_eq!(
            toks("#ff8000"),
            vec![Tok::HexColor([255, 128, 0, 255]), Tok::Newline, Tok::Eof]
        );
        assert_eq!(
            toks("#ff800080"),
            vec![Tok::HexColor([255, 128, 0, 128]), Tok::Newline, Tok::Eof]
        );
        // Seven digits is not a colour, so it is a comment.
        assert_eq!(toks("#ff80001"), vec![Tok::Eof]);
        // And a word after the digits makes it a comment too.
        assert_eq!(toks("#ff8000abcxyz"), vec![Tok::Eof]);
        assert_eq!(toks("# ff8000"), vec![Tok::Eof]);
    }

    #[test]
    fn numbers_carry_their_units() {
        // Units are part of the literal. `90deg` must not lex as `90` and an
        // identifier, or the classic unit bug walks straight in.
        assert_eq!(
            toks("1 2.5 1.2m 90deg 250ms 3hz 50%"),
            vec![
                num("1", None),
                num("2.5", None),
                num("1.2", Some(Unit::M)),
                num("90", Some(Unit::Deg)),
                num("250", Some(Unit::Ms)),
                num("3", Some(Unit::Hz)),
                num("50", Some(Unit::Percent)),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn every_unit_maps_both_ways() {
        for u in [
            Unit::M,
            Unit::S,
            Unit::Ms,
            Unit::Deg,
            Unit::Rad,
            Unit::Hz,
            Unit::Percent,
        ] {
            assert_eq!(Unit::from_str(u.as_str()), Some(u));
        }
        assert_eq!(Unit::from_str("furlong"), None);
    }

    #[test]
    fn an_unknown_unit_is_reported_rather_than_ignored() {
        let (_, errs) = lex("5furlongs");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unknown unit"));
    }

    #[test]
    fn a_range_is_not_a_decimal_point() {
        // `1..2` must not lex as `1.` `.2`. This is why the decimal point needs
        // a digit after it.
        assert_eq!(
            toks("1..2"),
            vec![
                num("1", None),
                Tok::DotDot,
                num("2", None),
                Tok::Newline,
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("0.0..1.0"),
            vec![
                num("0.0", None),
                Tok::DotDot,
                num("1.0", None),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn strings_handle_escapes() {
        assert_eq!(
            toks(r#""hello\nworld \"quoted\"""#),
            vec![
                Tok::Str("hello\nworld \"quoted\"".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn strings_carry_non_ascii_through_intact() {
        assert_eq!(
            toks("\"caf\u{e9} \u{1f525}\""),
            vec![
                Tok::Str("caf\u{e9} \u{1f525}".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn an_unterminated_string_is_reported_not_swallowed() {
        let (_, errs) = lex("\"no end");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unterminated"));

        let (_, errs2) = lex("\"no end\nnext line\"");
        assert!(!errs2.is_empty());
    }

    #[test]
    fn an_unknown_escape_is_reported() {
        let (_, errs) = lex(r#""bad \q escape""#);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("escape"));
    }

    #[test]
    fn newlines_end_statements_but_not_inside_brackets() {
        // An expression may wrap while brackets are open. Deciding that here
        // keeps "when does a statement end" in one place.
        assert_eq!(
            toks("a\nb"),
            vec![
                Tok::Ident("a".into()),
                Tok::Newline,
                Tok::Ident("b".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("f(a,\n  b)"),
            vec![
                Tok::Ident("f".into()),
                Tok::LParen,
                Tok::Ident("a".into()),
                Tok::Comma,
                Tok::Ident("b".into()),
                Tok::RParen,
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn runs_of_blank_lines_collapse() {
        assert_eq!(
            toks("a\n\n\n\nb"),
            vec![
                Tok::Ident("a".into()),
                Tok::Newline,
                Tok::Ident("b".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
        assert_eq!(toks("\n\n\n"), vec![Tok::Eof]);
    }

    #[test]
    fn braces_do_not_suppress_newlines() {
        // Statements inside a block still end at a newline; only round and
        // square brackets continue an expression.
        assert_eq!(
            toks("{\na\n}"),
            vec![
                Tok::LBrace,
                Tok::Newline,
                Tok::Ident("a".into()),
                Tok::Newline,
                Tok::RBrace,
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn lexes_every_operator() {
        assert_eq!(
            toks("+ - * / % < <= > >= == != && || ! = -> . .. , : ( ) [ ] { }"),
            vec![
                Tok::Plus,
                Tok::Minus,
                Tok::Star,
                Tok::Slash,
                Tok::Percent,
                Tok::Lt,
                Tok::Le,
                Tok::Gt,
                Tok::Ge,
                Tok::EqEq,
                Tok::Ne,
                Tok::AndAnd,
                Tok::OrOr,
                Tok::Bang,
                Tok::Assign,
                Tok::Arrow,
                Tok::Dot,
                Tok::DotDot,
                Tok::Comma,
                Tok::Colon,
                Tok::LParen,
                Tok::RParen,
                Tok::LBracket,
                Tok::RBracket,
                Tok::LBrace,
                Tok::RBrace,
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn an_unexpected_character_is_reported() {
        let (_, errs) = lex("a $ b");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unexpected character"));
    }

    #[test]
    fn spans_point_at_the_right_text() {
        let src = "effect \"x\"";
        let (t, _) = lex(src);
        assert_eq!(&src[t[0].span.range()], "effect");
        assert_eq!(&src[t[1].span.range()], "\"x\"");
    }

    #[test]
    fn every_token_describes_itself_for_diagnostics() {
        let (t, _) = lex("a 1 #ff0000 \"s\" + { ( [ , : . .. = -> < <= > >= == != && || !");
        for tok in &t {
            assert!(!tok.tok.describe().is_empty());
        }
    }
}
