use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{SourceId, SymbolId};
use crate::common::span::Span;
use crate::common::symbols::Interner;

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
                b'0'..=b'9' => self.lex_integer(),
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

    fn lex_integer(&mut self) {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        let bytes = &self.bytes[start..self.offset];
        let text = String::from_utf8_lossy(bytes);
        match text.parse::<i64>() {
            Ok(value) => self.tokens.push(Token {
                kind: TokenKind::Integer(value),
                span: self.span(start, self.offset),
            }),
            Err(_) => {
                self.diagnostics.error(
                    "LEX_BAD_INT",
                    format!("Invalid integer literal '{text}'"),
                    self.span(start, self.offset),
                );
            }
        }
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
