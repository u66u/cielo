use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{Interner, SourceId, Span, SymbolId};

macro_rules! define_keywords {
    ($($text:literal => $variant:ident),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Keyword {
            $($variant),*
        }

        fn from_str(text: &str) -> Option<Keyword> {
            match text {
                $($text => Some(Keyword::$variant),)*
                _ => None,
            }
        }
    };
}

define_keywords! {
    "fn" => Fn,
    "struct" => Struct,
    "enum" => Enum,
    "effect" => Effect,
    "handle" => Handle,
    "handler" => Handler,
    "do" => Do,
    "let" => Let,
    "if" => If,
    "match" => Match,
    "else" => Else,
    "true" => True,
    "false" => False,
    "with" => With,
    "comptime" => Comptime,
    "runtime" => Runtime,
}

#[derive(Clone, PartialEq, Debug)]
pub enum TokenKind {
    Identifier(SymbolId),
    Integer(i64),
    Float(f64),
    Char(char),
    String(String),
    Keyword(Keyword),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semi,
    Dot,
    Arrow,
    FatArrow,
    Eq,
    EqEq,
    Bang,
    BangEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    Pipe,
    OrOr,
    At,
    Eof,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: DiagnosticBag,
}

pub fn lex(source: &str, source_id: SourceId, interner: &mut Interner) -> LexOutput {
    let mut lexer = Lexer::new(source, source_id, interner);
    lexer.run();
    LexOutput {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

struct Lexer<'a> {
    bytes: &'a [u8],
    source_id: SourceId,
    offset: usize,
    tokens: Vec<Token>,
    diagnostics: DiagnosticBag,
    interner: &'a mut Interner,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str, source_id: SourceId, interner: &'a mut Interner) -> Self {
        Self {
            bytes: source.as_bytes(),
            source_id,
            offset: 0,
            tokens: Vec::new(),
            diagnostics: DiagnosticBag::default(),
            interner,
        }
    }

    fn run(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => self.offset += 1,
                b'/' if self.peek_n(1) == Some(b'/') => self.skip_line_comment(),
                b'-' if self.peek_n(1) == Some(b'-') => self.skip_line_comment(),
                b'(' => self.single(TokenKind::LParen),
                b')' => self.single(TokenKind::RParen),
                b'{' => self.single(TokenKind::LBrace),
                b'}' => self.single(TokenKind::RBrace),
                b'[' => self.single(TokenKind::LBracket),
                b']' => self.single(TokenKind::RBracket),
                b',' => self.single(TokenKind::Comma),
                b':' => self.single(TokenKind::Colon),
                b';' => self.single(TokenKind::Semi),
                b'.' => self.single(TokenKind::Dot),
                b'@' => self.single(TokenKind::At),
                b'+' => self.single(TokenKind::Plus),
                b'*' => self.single(TokenKind::Star),
                b'%' => self.single(TokenKind::Percent),
                b'=' => {
                    if self.peek_n(1) == Some(b'=') {
                        self.double(TokenKind::EqEq)
                    } else if self.peek_n(1) == Some(b'>') {
                        self.double(TokenKind::FatArrow)
                    } else {
                        self.single(TokenKind::Eq)
                    }
                }
                b'!' => {
                    if self.peek_n(1) == Some(b'=') {
                        self.double(TokenKind::BangEq)
                    } else {
                        self.single(TokenKind::Bang)
                    }
                }
                b'<' => {
                    if self.peek_n(1) == Some(b'=') {
                        self.double(TokenKind::Le)
                    } else {
                        self.single(TokenKind::Lt)
                    }
                }
                b'>' => {
                    if self.peek_n(1) == Some(b'=') {
                        self.double(TokenKind::Ge)
                    } else {
                        self.single(TokenKind::Gt)
                    }
                }
                b'&' if self.peek_n(1) == Some(b'&') => self.double(TokenKind::AndAnd),
                b'|' if self.peek_n(1) == Some(b'|') => self.double(TokenKind::OrOr),
                b'|' => self.single(TokenKind::Pipe),
                b'-' if self.peek_n(1) == Some(b'>') => self.double(TokenKind::Arrow),
                b'-' => self.single(TokenKind::Minus),
                b'/' => self.single(TokenKind::Slash),
                b'"' => self.lex_string(),
                b'\'' => self.lex_char(),
                b'0'..=b'9' => self.lex_number(),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_identifier_or_keyword(),
                _ => {
                    let span = self.span(self.offset, self.offset + 1);
                    self.diagnostics.error(
                        "LEX_UNEXPECTED_CHAR",
                        format!("Unexpected character '{}'", byte as char),
                        span,
                    );
                    self.offset += 1;
                }
            }
        }

        let eof_span = self.span(self.offset, self.offset);
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: eof_span,
        });
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn peek_n(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.offset + n).copied()
    }

    fn single(&mut self, kind: TokenKind) {
        let start = self.offset;
        self.offset += 1;
        self.tokens.push(Token {
            kind,
            span: self.span(start, self.offset),
        });
    }

    fn double(&mut self, kind: TokenKind) {
        let start = self.offset;
        self.offset += 2;
        self.tokens.push(Token {
            kind,
            span: self.span(start, self.offset),
        });
    }

    fn lex_string(&mut self) {
        let start = self.offset;
        self.offset += 1;
        let mut escaped = false;

        while let Some(byte) = self.peek() {
            self.offset += 1;
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => {
                    let raw = &self.bytes[start + 1..self.offset - 1];
                    let text = String::from_utf8_lossy(raw).into_owned();
                    self.tokens.push(Token {
                        kind: TokenKind::String(text),
                        span: self.span(start, self.offset),
                    });
                    return;
                }
                _ => {}
            }
        }

        self.diagnostics.error(
            "LEX_UNTERMINATED_STRING",
            "Unterminated string literal",
            self.span(start, self.offset),
        );
    }

    /// A `Char` is one Unicode scalar value, not one byte: `'é'` and `'字'` are
    /// single characters here and reach the runtime as their code point. The
    /// whole escape set is `\n`, `\t`, `\r`, `\\`, `\'` and `\0`; anything else
    /// after a backslash is an error rather than the escaped byte, so a typo
    /// cannot quietly become a different character.
    fn lex_char(&mut self) {
        let start = self.offset;
        self.offset += 1;
        let body_start = self.offset;

        // A backslash always consumes the next byte, so the closing quote of
        // `'\''` is the fourth byte and not the third. Neither `\` nor `'` can
        // occur inside a multi-byte UTF-8 sequence, so scanning bytes cannot
        // stop in the middle of a character.
        let mut body_end = None;
        while let Some(byte) = self.peek() {
            match byte {
                b'\\' => self.offset = (self.offset + 2).min(self.bytes.len()),
                b'\'' => {
                    body_end = Some(self.offset);
                    self.offset += 1;
                    break;
                }
                _ => self.offset += 1,
            }
        }

        let Some(body_end) = body_end else {
            self.diagnostics.error(
                "LEX_UNTERMINATED_CHAR",
                "Unterminated character literal",
                self.span(start, self.offset),
            );
            return;
        };

        let span = self.span(start, self.offset);
        let body = &self.bytes[body_start..body_end];
        if body.is_empty() {
            self.diagnostics.error(
                "LEX_EMPTY_CHAR",
                "Empty character literal: a character literal holds exactly one character",
                span,
            );
            return;
        }

        // The source arrived as `&str`, so the body is always valid UTF-8 and
        // the lossy decode never substitutes anything.
        let (value, rest) = if body[0] == b'\\' {
            let decoded = match body.get(1) {
                Some(b'n') => '\n',
                Some(b't') => '\t',
                Some(b'r') => '\r',
                Some(b'\\') => '\\',
                Some(b'\'') => '\'',
                Some(b'0') => '\0',
                _ => {
                    let text = String::from_utf8_lossy(body);
                    self.diagnostics.error(
                        "LEX_BAD_ESCAPE",
                        format!(
                            "Unknown escape '{text}' in a character literal: the escapes are \\n, \\t, \\r, \\\\, \\' and \\0"
                        ),
                        span,
                    );
                    return;
                }
            };
            (decoded, &body[2..])
        } else {
            let text = String::from_utf8_lossy(body);
            let mut chars = text.chars();
            let first = chars.next().expect("a non-empty body decodes to a char");
            let consumed = first.len_utf8();
            (first, &body[consumed..])
        };

        if !rest.is_empty() {
            let text = String::from_utf8_lossy(body);
            self.diagnostics.error(
                "LEX_MULTI_CHAR",
                format!(
                    "Character literal '{text}' holds more than one character: use a string literal \"{text}\" instead"
                ),
                span,
            );
            return;
        }

        self.tokens.push(Token {
            kind: TokenKind::Char(value),
            span,
        });
    }

    /// Accepted: `1`, `1.5`, `1e9`, `1.5e-3`. Rejected: `1.`, `1.foo`, `.5`,
    /// `1e`. A `.` after a digit run is only ever a float point, because an
    /// integer has no fields for `1.foo` to project; consuming it only when a
    /// digit follows is what keeps `p.a` field access unambiguous. `.5` never
    /// reaches here at all -- the leading `.` would already have been taken as
    /// a field access on whatever preceded it.
    fn lex_number(&mut self) {
        let start = self.offset;
        self.eat_digits();
        let mut is_float = false;

        if self.peek() == Some(b'.') && matches!(self.peek_n(1), Some(b'0'..=b'9')) {
            self.offset += 1;
            self.eat_digits();
            is_float = true;
        }

        if matches!(self.peek(), Some(b'e' | b'E')) && self.exponent_digits_follow() {
            self.offset += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            self.eat_digits();
            is_float = true;
        }

        // A literal must end on a boundary. Letting `1.`, `1.foo` or `1e` split
        // into several tokens reports the mistake somewhere unrelated, so the
        // trailing run is swallowed and blamed on the literal itself.
        if self.at_number_tail() {
            while self.at_number_tail() {
                self.offset += 1;
            }
            let text = String::from_utf8_lossy(&self.bytes[start..self.offset]);
            self.diagnostics.error(
                "LEX_BAD_NUMBER",
                format!("Invalid numeric literal '{text}'"),
                self.span(start, self.offset),
            );
            return;
        }

        let span = self.span(start, self.offset);
        let text = String::from_utf8_lossy(&self.bytes[start..self.offset]);
        if is_float {
            // Overflow parses as infinity rather than failing, and every stage
            // below treats a non-finite float as unfoldable and unpoolable, so
            // a mistyped exponent would silently become a value nothing folds.
            match text.parse::<f64>() {
                Ok(value) if value.is_finite() => self.tokens.push(Token {
                    kind: TokenKind::Float(value),
                    span,
                }),
                _ => {
                    self.diagnostics.error(
                        "LEX_BAD_FLOAT",
                        format!("Float literal '{text}' is not a finite number"),
                        span,
                    );
                }
            }
            return;
        }

        match text.parse::<i64>() {
            Ok(value) => self.tokens.push(Token {
                kind: TokenKind::Integer(value),
                span,
            }),
            Err(_) => {
                self.diagnostics.error(
                    "LEX_BAD_INT",
                    format!("Invalid integer literal '{text}'"),
                    span,
                );
            }
        }
    }

    fn eat_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
    }

    fn exponent_digits_follow(&self) -> bool {
        let after_sign = if matches!(self.peek_n(1), Some(b'+' | b'-')) {
            2
        } else {
            1
        };
        matches!(self.peek_n(after_sign), Some(b'0'..=b'9'))
    }

    fn at_number_tail(&self) -> bool {
        matches!(
            self.peek(),
            Some(b'.' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        )
    }

    fn lex_identifier_or_keyword(&mut self) {
        let start = self.offset;
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.offset += 1;
        }

        let text = String::from_utf8_lossy(&self.bytes[start..self.offset]);
        if let Some(keyword) = from_str(&text) {
            self.tokens.push(Token {
                kind: TokenKind::Keyword(keyword),
                span: self.span(start, self.offset),
            });
            return;
        }

        let id = self.interner.intern(&text);
        self.tokens.push(Token {
            kind: TokenKind::Identifier(id),
            span: self.span(start, self.offset),
        });
    }

    fn skip_line_comment(&mut self) {
        while let Some(byte) = self.peek() {
            self.offset += 1;
            if byte == b'\n' {
                break;
            }
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.source_id, start as u32, end as u32)
    }
}
