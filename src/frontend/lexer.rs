use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{SourceId, SymbolId};
use crate::common::span::Span;
use crate::common::symbols::Interner;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyword {
    Fn,
    Struct,
    Enum,
    Effect,
    Let,
    If,
    Else,
    True,
    False,
    With,
    Comptime,
    Runtime,
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
