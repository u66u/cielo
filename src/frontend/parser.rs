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
                }
                self.expect_kind(TokenKind::RParen, "Expected `)` after enum variant fields");
            }

            let span = span_join(variant_start, self.prev_span());
            variants.push(EnumVariantDecl {
                name: variant_name,
                fields,
                span,
            });

            if self.consume_kind(TokenKind::Comma).is_none() {
                break;
            }
        }

        let end = self
            .expect_kind(TokenKind::RBrace, "Expected `}` to close enum")
            .span;
        EnumDecl {
            name,
            variants,
            span: span_join(start, end),
        }
    }

    fn parse_effect(&mut self) -> EffectDecl {
        let start = self.expect_keyword(Keyword::Effect).span;
        let name = self.expect_identifier("Expected effect name after `effect`");
        self.expect_kind(TokenKind::LBrace, "Expected `{` after effect name");

        let mut operations = Vec::new();
        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            let op_start = self.current_span();
            self.expect_keyword(Keyword::Fn);
            let op_name = self.expect_identifier("Expected effect operation name");
            self.expect_kind(TokenKind::LParen, "Expected `(` after operation name");
            let params = self.parse_params();
            self.expect_kind(TokenKind::RParen, "Expected `)` after operation parameters");
            let return_type = if self.consume_kind(TokenKind::Arrow).is_some() {
                Some(self.parse_type_expr())
            } else {
                None
            };
            let span = span_join(op_start, self.prev_span());
            operations.push(EffectOperationDecl {
                name: op_name,
                params,
                return_type,
                span,
            });
            self.consume_kind(TokenKind::Semi);
        }

        let end = self
            .expect_kind(TokenKind::RBrace, "Expected `}` to close effect")
            .span;
        EffectDecl {
            name,
            operations,
            span: span_join(start, end),
        }
    }

    fn parse_params(&mut self) -> Vec<Param> {
        let mut params = Vec::new();
        if self.check_kind(TokenKind::RParen) {
            return params;
        }

        loop {
            let start = self.current_span();
            let name = self.expect_identifier("Expected parameter name");
            self.expect_kind(TokenKind::Colon, "Expected `:` after parameter name");
            let ty = self.parse_type_expr();
            let span = span_join(start, ty.span);
            params.push(Param { name, ty, span });
            if self.consume_kind(TokenKind::Comma).is_none() {
                break;
            }
        }
        params
    }

    fn parse_type_expr(&mut self) -> TypeExpr {
        if self.consume_kind(TokenKind::LParen).is_some() {
            let left = self.prev_span();
            self.expect_kind(TokenKind::RParen, "Expected `)` for unit type");
            return TypeExpr {
                kind: TypeExprKind::Unit,
                span: span_join(left, self.prev_span()),
            };
        }

        let start = self.current_span();
        let name = self.expect_identifier("Expected type name");
        let mut args = Vec::new();
        if self.consume_kind(TokenKind::LBracket).is_some() {
            if !self.check_kind(TokenKind::RBracket) {
                loop {
                    args.push(self.parse_type_expr());
                    if self.consume_kind(TokenKind::Comma).is_none() {
                        break;
                    }
                }
            }
            self.expect_kind(
                TokenKind::RBracket,
                "Expected `]` after generic type arguments",
            );
        }

        TypeExpr {
            kind: TypeExprKind::Path { name, args },
            span: span_join(start, self.prev_span()),
        }
    }

    fn parse_block(&mut self) -> BlockExpr {
        let start = self
            .expect_kind(TokenKind::LBrace, "Expected block `{`")
            .span;
        let mut statements = Vec::new();
        let mut tail = None;

        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            if self.check_keyword(Keyword::Let) {
                statements.push(self.parse_let_stmt());
                self.consume_kind(TokenKind::Semi);
                continue;
            }
            if self.check_keyword(Keyword::Do) {
                statements.push(self.parse_perform_stmt());
                self.consume_kind(TokenKind::Semi);
                continue;
            }

            let expr = self.parse_expr(0);
            if self.consume_kind(TokenKind::Semi).is_some() {
                let span = expr.span;
                statements.push(Stmt::Expr { value: expr, span });
                continue;
            }

            if self.check_kind(TokenKind::RBrace) {
                tail = Some(Box::new(expr));
                break;
            }

            if self.check_keyword(Keyword::Let) {
                let span = expr.span;
                statements.push(Stmt::Expr { value: expr, span });
                continue;
            }

            let span = self.current_span();
            self.diagnostics.error(
                "PARSE_EXPECTED_SEMI",
                "Expected `;` or `}` after expression",
                span,
            );
            let stmt_span = expr.span;
            statements.push(Stmt::Expr {
                value: expr,
                span: stmt_span,
            });
            self.recover_stmt_boundary();
            self.consume_kind(TokenKind::Semi);
        }

        let end = self
            .expect_kind(TokenKind::RBrace, "Expected `}` to close block")
            .span;
        BlockExpr {
            statements,
            tail,
            span: span_join(start, end),
        }
    }

    fn parse_let_stmt(&mut self) -> Stmt {
        let start = self.expect_keyword(Keyword::Let).span;
        let name = self.expect_identifier("Expected variable name after `let`");
        let ty = if self.consume_kind(TokenKind::Colon).is_some() {
            Some(self.parse_type_expr())
        } else {
            None
        };
        self.expect_kind(TokenKind::Eq, "Expected `=` in `let` binding");
        let value = self.parse_expr(0);
        let span = span_join(start, value.span);
        Stmt::Let {
            name,
            ty,
            value,
            span,
        }
    }

    fn parse_perform_stmt(&mut self) -> Stmt {
        let start = self.expect_keyword(Keyword::Do).span;
        let effect = self.expect_identifier("Expected effect name after `do`");
        self.expect_kind(TokenKind::Dot, "Expected `.` after effect name in `do` statement");
        let operation = self.expect_identifier("Expected operation name after effect in `do` statement");
        self.expect_kind(TokenKind::LParen, "Expected `(` after effect operation name");
        let mut args = Vec::new();
        if !self.check_kind(TokenKind::RParen) {
            loop {
                args.push(self.parse_expr(0));
                if self.consume_kind(TokenKind::Comma).is_none() {
                    break;
                }
            }
        }
        let end = self.expect_kind(TokenKind::RParen, "Expected `)` after effect operation arguments");
        Stmt::Perform {
            effect,
            operation,
            args,
            span: span_join(start, end.span),
        }
    }

    fn parse_expr(&mut self, min_prec: u8) -> Expr {
        let mut lhs = self.parse_prefix_expr();
        loop {
            if self.check_kind(TokenKind::LParen) {
                lhs = self.parse_call_expr(lhs);
                continue;
            }

            let Some((op, precedence)) = self.peek_binop() else {
                break;
            };
            if precedence < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_expr(precedence + 1);
            let span = span_join(lhs.span, rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        lhs
    }

    fn parse_prefix_expr(&mut self) -> Expr {
        let token = self.current().clone();
        match token.kind {
            TokenKind::Integer(value) => {
                self.bump();
                Expr {
                    kind: ExprKind::Int(value),
                    span: token.span,
                }
            }
            TokenKind::String(ref value) => {
                self.bump();
                Expr {
                    kind: ExprKind::String(value.clone()),
                    span: token.span,
                }
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Expr {
                    kind: ExprKind::Bool(true),
                    span: token.span,
                }
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Expr {
                    kind: ExprKind::Bool(false),
                    span: token.span,
                }
            }
            TokenKind::Identifier(name) => {
                self.bump();
                Expr {
                    kind: ExprKind::Var(name),
                    span: token.span,
                }
            }
            TokenKind::LParen => {
                self.bump();
                let expr = self.parse_expr(0);
                self.expect_kind(TokenKind::RParen, "Expected `)` after expression");
                expr
            }
            TokenKind::Keyword(Keyword::If) => self.parse_if_expr(),
            TokenKind::LBrace => {
                let block = self.parse_block();
                Expr {
                    span: block.span,
                    kind: ExprKind::Block(block),
                }
            }
            TokenKind::At => self.parse_stage_expr(),
            TokenKind::Minus => {
                self.bump();
                let expr = self.parse_expr(7);
                let span = span_join(token.span, expr.span);
                Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Neg,
                        expr: Box::new(expr),
                    },
                    span,
                }
            }
            TokenKind::Bang => {
                self.bump();
                let expr = self.parse_expr(7);
                let span = span_join(token.span, expr.span);
                Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(expr),
                    },
                    span,
                }
            }
            _ => {
                let span = token.span;
                let err =
                    self.diagnostics
                        .error_node("PARSE_EXPR_EXPECTED", "Expected expression", span);
                self.bump();
                Expr {
                    kind: ExprKind::Error(err),
                    span,
                }
            }
        }
    }

    fn parse_call_expr(&mut self, callee: Expr) -> Expr {
        self.expect_kind(TokenKind::LParen, "Expected `(` for call");
        let mut args = Vec::new();
        if !self.check_kind(TokenKind::RParen) {
            loop {
                args.push(self.parse_expr(0));
                if self.consume_kind(TokenKind::Comma).is_none() {
                    break;
                }
            }
        }
        let end = self.expect_kind(TokenKind::RParen, "Expected `)` after call arguments");
        let span = span_join(callee.span, end.span);
        Expr {
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
            span,
        }
    }

    fn parse_if_expr(&mut self) -> Expr {
        let start = self.expect_keyword(Keyword::If).span;
        let cond = self.parse_expr(0);
        let then_branch = self.parse_block();
        let else_branch = if self.consume_keyword(Keyword::Else).is_some() {
            if self.check_keyword(Keyword::If) {
                let nested_if = self.parse_if_expr();
                Some(BlockExpr {
                    span: nested_if.span,
                    statements: Vec::new(),
                    tail: Some(Box::new(nested_if)),
                })
            } else {
                Some(self.parse_block())
            }
        } else {
            None
        };

        let end_span = else_branch
            .as_ref()
            .map_or(then_branch.span, |block| block.span);
        Expr {
            kind: ExprKind::If {
                cond: Box::new(cond),
                then_branch,
                else_branch,
            },
            span: span_join(start, end_span),
        }
    }

    fn parse_stage_expr(&mut self) -> Expr {
        let start = self.expect_kind(TokenKind::At, "Expected `@`").span;
        let stage = if self.consume_keyword(Keyword::Comptime).is_some() {
            StageMarker::Comptime
        } else if self.consume_keyword(Keyword::Runtime).is_some() {
            StageMarker::Runtime
        } else {
            self.diagnostics.error(
                "PARSE_STAGE_MARKER",
                "Expected `comptime` or `runtime` after `@`",
                self.current_span(),
            );
            StageMarker::Runtime
        };
        let block = self.parse_block();
        Expr {
            kind: ExprKind::StageBlock { stage, block },
            span: span_join(start, self.prev_span()),
        }
    }

    fn peek_binop(&self) -> Option<(BinOp, u8)> {
        let op = match self.current().kind {
            TokenKind::Plus => BinOp::Add,
            TokenKind::Minus => BinOp::Sub,
            TokenKind::Star => BinOp::Mul,
            TokenKind::Slash => BinOp::Div,
            TokenKind::Percent => BinOp::Mod,
            TokenKind::EqEq => BinOp::Eq,
            TokenKind::BangEq => BinOp::Ne,
            TokenKind::Lt => BinOp::Lt,
            TokenKind::Le => BinOp::Le,
            TokenKind::Gt => BinOp::Gt,
            TokenKind::Ge => BinOp::Ge,
            TokenKind::AndAnd => BinOp::And,
            TokenKind::OrOr => BinOp::Or,
            _ => return None,
        };
        Some((op, op.precedence()))
    }

    fn recover_item(&mut self) {
        while !self.at_eof() {
            if self.check_keyword(Keyword::Fn)
                || self.check_keyword(Keyword::Struct)
                || self.check_keyword(Keyword::Enum)
                || self.check_keyword(Keyword::Effect)
            {
                return;
            }
            self.bump();
        }
    }

    fn recover_stmt_boundary(&mut self) {
        while !self.at_eof() {
            if self.check_kind(TokenKind::Semi)
                || self.check_kind(TokenKind::RBrace)
                || self.check_keyword(Keyword::Let)
            {
                return;
            }
            self.bump();
        }
    }

    fn current(&self) -> &Token {
        self.tokens
            .get(self.index)
            .unwrap_or_else(|| self.tokens.last().expect("token stream has EOF"))
    }

    fn current_span(&self) -> Span {
        self.current().span
    }

    fn prev_span(&self) -> Span {
        if self.index == 0 {
            self.current_span()
        } else {
            self.tokens[self.index - 1].span
        }
    }

    fn bump(&mut self) -> Token {
        let token = self.current().clone();
        if !self.at_eof() {
            self.index += 1;
        }
        token
    }

    fn at_eof(&self) -> bool {
        matches!(self.current().kind, TokenKind::Eof)
    }

    fn check_kind(&self, kind: TokenKind) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(&kind)
    }

    fn check_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.current().kind, TokenKind::Keyword(k) if k == keyword)
    }

    fn consume_kind(&mut self, kind: TokenKind) -> Option<Token> {
        if self.check_kind(kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn consume_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        if self.check_keyword(keyword) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn expect_kind(&mut self, kind: TokenKind, message: &str) -> Token {
        if self.check_kind(kind) {
            return self.bump();
        }
        let span = self.current_span();
        self.diagnostics
            .error("PARSE_TOKEN_EXPECTED", message.to_owned(), span);
        self.bump()
    }

    fn expect_keyword(&mut self, keyword: Keyword) -> Token {
        if self.check_keyword(keyword) {
            return self.bump();
        }
        let span = self.current_span();
        self.diagnostics.error(
            "PARSE_KEYWORD_EXPECTED",
            format!("Expected keyword `{keyword:?}`"),
            span,
        );
        self.bump()
    }

    fn expect_identifier(&mut self, message: &str) -> SymbolId {
        match self.current().kind {
            TokenKind::Identifier(id) => {
                self.bump();
                id
            }
            _ => {
                let span = self.current_span();
                self.diagnostics
                    .error("PARSE_IDENT_EXPECTED", message.to_owned(), span);
                self.bump();
                SymbolId::INVALID
            }
        }
    }
}

fn span_join(left: Span, right: Span) -> Span {
    if left.source != right.source {
        return left;
    }
    Span::new(
        left.source,
        left.start.min(right.start),
        left.end.max(right.end),
    )
}
