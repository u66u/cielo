use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{SourceId, SymbolId};
use crate::common::span::Span;
use crate::common::symbols::Interner;
use crate::frontend::ast::{
    BinOp, BlockExpr, EffectDecl, EffectOperationDecl, EnumDecl, EnumVariantDecl, Expr, ExprKind,
    FieldDecl, FunctionDecl, Item, Param, Program, StageMarker, Stmt, StructDecl, TypeExpr,
    TypeExprKind, UnaryOp,
};
use crate::frontend::lexer::{Keyword, Token, TokenKind, lex};

#[derive(Clone, Debug)]
pub struct ParseOutput {
    pub program: Program,
    pub diagnostics: DiagnosticBag,
}

pub fn parse_source(source: &str, source_id: SourceId, interner: &mut Interner) -> ParseOutput {
    let lexed = lex(source, source_id, interner);
    let mut parser = Parser::new(lexed.tokens, lexed.diagnostics);
    let program = parser.parse_program();
    ParseOutput {
        program,
        diagnostics: parser.diagnostics,
    }
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    diagnostics: DiagnosticBag,
}

impl Parser {
    fn new(tokens: Vec<Token>, diagnostics: DiagnosticBag) -> Self {
        Self {
            tokens,
            index: 0,
            diagnostics,
        }
    }

    fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        while !self.at_eof() {
            if self.check_kind(TokenKind::Eof) {
                break;
            }

            if self.check_keyword(Keyword::Fn) {
                items.push(Item::Function(self.parse_function()));
                continue;
            }
            if self.check_keyword(Keyword::Struct) {
                items.push(Item::Struct(self.parse_struct()));
                continue;
            }
            if self.check_keyword(Keyword::Enum) {
                items.push(Item::Enum(self.parse_enum()));
                continue;
            }
            if self.check_keyword(Keyword::Effect) {
                items.push(Item::Effect(self.parse_effect()));
                continue;
            }

            let span = self.current_span();
            let err = self.diagnostics.error_node(
                "PARSE_ITEM_EXPECTED",
                "Expected top-level item (`fn`, `struct`, `enum`, or `effect`)",
                span,
            );
            items.push(Item::Error(err));
            self.recover_item();
        }
        Program { items }
    }

    fn parse_function(&mut self) -> FunctionDecl {
        let start = self.expect_keyword(Keyword::Fn).span;
        let name = self.expect_identifier("Expected function name after `fn`");
        self.expect_kind(TokenKind::LParen, "Expected `(` after function name");
        let params = self.parse_params();
        self.expect_kind(TokenKind::RParen, "Expected `)` after function parameters");

        let return_type = if self.consume_kind(TokenKind::Arrow).is_some() {
            Some(self.parse_type_expr())
        } else {
            None
        };

        let mut effects = Vec::new();
        if self.consume_keyword(Keyword::With).is_some() {
            loop {
                effects.push(self.expect_identifier("Expected effect name in `with` clause"));
                if self.consume_kind(TokenKind::Comma).is_none() {
                    break;
                }
            }
        }

        let body = self.parse_block();
        let span = span_join(start, body.span);
        FunctionDecl {
            name,
            params,
            return_type,
            effects,
            body,
            span,
        }
    }

    fn parse_struct(&mut self) -> StructDecl {
        let start = self.expect_keyword(Keyword::Struct).span;
        let name = self.expect_identifier("Expected struct name after `struct`");
        self.expect_kind(TokenKind::LBrace, "Expected `{` after struct name");

        let mut fields = Vec::new();
        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            let field_name = self.expect_identifier("Expected field name");
            self.expect_kind(TokenKind::Colon, "Expected `:` after field name");
            let field_type = self.parse_type_expr();
            let span = span_join(self.prev_span(), field_type.span);
            fields.push(FieldDecl {
                name: field_name,
                ty: field_type,
                span,
            });

            if self.consume_kind(TokenKind::Comma).is_none() {
                break;
            }
        }

        let end = self
            .expect_kind(TokenKind::RBrace, "Expected `}` to close struct")
            .span;
        StructDecl {
            name,
            fields,
            span: span_join(start, end),
        }
    }

    fn parse_enum(&mut self) -> EnumDecl {
        let start = self.expect_keyword(Keyword::Enum).span;
        let name = self.expect_identifier("Expected enum name after `enum`");
        self.expect_kind(TokenKind::LBrace, "Expected `{` after enum name");

        let mut variants = Vec::new();
        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            let variant_start = self.current_span();
            let variant_name = self.expect_identifier("Expected enum variant name");
            let mut fields = Vec::new();

            if self.consume_kind(TokenKind::LParen).is_some() {
                if !self.check_kind(TokenKind::RParen) {
                    loop {
                        fields.push(self.parse_type_expr());
                        if self.consume_kind(TokenKind::Comma).is_none() {
                            break;
                        }
                    }
