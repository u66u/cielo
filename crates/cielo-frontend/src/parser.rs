use crate::ast::{
    BinOp, BlockExpr, BuiltinType, EffectCapabilityHint, EffectDecl, EffectOperationDecl,
    EffectPropertyHint, EnumDecl, EnumVariantDecl, Expr, ExprKind, FieldDecl, FunctionDecl,
    HandleClause, Item, MatchClause, Param, Program, StageMarker, Stmt, StructDecl, TypeExpr,
    TypeExprKind, UnaryOp,
};
use crate::lexer::{Keyword, Token, TokenKind, lex};
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{Interner, SourceId, Span, SymbolId};

macro_rules! map_binops {
    ($tok:expr, $($pat:pat => $op:expr),+ $(,)?) => {
        match $tok {
            $($pat => Some(($op, $op.precedence())),)+
            _ => None,
        }
    };
}

#[derive(Clone, Debug)]
pub struct ParseOutput {
    pub program: Program,
    pub diagnostics: DiagnosticBag,
}

impl ParseOutput {
    pub fn new(program: Program, diagnostics: DiagnosticBag) -> Self {
        Self {
            program,
            diagnostics,
        }
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn into_parts(self) -> (Program, DiagnosticBag) {
        (self.program, self.diagnostics)
    }
}

pub fn parse_source(source: &str, source_id: SourceId, interner: &mut Interner) -> ParseOutput {
    let builtins = BuiltinTypeSymbols::intern(interner);
    let effect_builtins = BuiltinEffectSymbols::intern(interner);
    let wildcard_symbol = interner.intern("_");
    let lexed = lex(source, source_id, interner);
    let mut parser = Parser::new(
        lexed.tokens,
        lexed.diagnostics,
        builtins,
        effect_builtins,
        wildcard_symbol,
    );
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
    builtins: BuiltinTypeSymbols,
    effect_builtins: BuiltinEffectSymbols,
    wildcard_symbol: SymbolId,
}

impl Parser {
    fn new(
        tokens: Vec<Token>,
        diagnostics: DiagnosticBag,
        builtins: BuiltinTypeSymbols,
        effect_builtins: BuiltinEffectSymbols,
        wildcard_symbol: SymbolId,
    ) -> Self {
        Self {
            tokens,
            index: 0,
            diagnostics,
            builtins,
            effect_builtins,
            wildcard_symbol,
        }
    }

    fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        while !self.at_eof() {
            if self.check_kind(TokenKind::Eof) {
                break;
            }

            if self.check_kind(TokenKind::At) {
                let at_span = self.expect_kind(TokenKind::At, "Expected `@`").span;
                if self.consume_keyword(Keyword::Comptime).is_none() {
                    self.diagnostics.error(
                        "PARSE_TOPLEVEL_ANNOT",
                        "Only `@comptime fn ...` is supported at top level in v1",
                        at_span,
                    );
                    self.recover_item();
                    continue;
                }
                if !self.check_keyword(Keyword::Fn) {
                    self.diagnostics.error(
                        "PARSE_TOPLEVEL_ANNOT_TARGET",
                        "`@comptime` can only annotate a function declaration",
                        at_span,
                    );
                    self.recover_item();
                    continue;
                }
                items.push(Item::Function(self.parse_function(true)));
                continue;
            }

            if self.check_keyword(Keyword::Fn) {
                items.push(Item::Function(self.parse_function(false)));
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

    fn parse_function(&mut self, ct_only: bool) -> FunctionDecl {
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
            ct_only,
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
            properties: self.effect_builtins.classify(name),
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

        if args.is_empty()
            && let Some(builtin) = self.builtins.resolve(name)
        {
            return TypeExpr {
                kind: TypeExprKind::Builtin(builtin),
                span: span_join(start, self.prev_span()),
            };
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
                statements.push(self.parse_perform_stmt(None));
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
        if self.check_keyword(Keyword::Do) {
            return self.parse_perform_stmt(Some(name));
        }
        let value = self.parse_expr(0);
        let span = span_join(start, value.span);
        Stmt::Let {
            name,
            ty,
            value,
            span,
        }
    }

    fn parse_perform_stmt(&mut self, binding: Option<SymbolId>) -> Stmt {
        let start = self.expect_keyword(Keyword::Do).span;
        let effect = self.expect_identifier("Expected effect name after `do`");
        self.expect_kind(
            TokenKind::Dot,
            "Expected `.` after effect name in `do` statement",
        );
        let operation =
            self.expect_identifier("Expected operation name after effect in `do` statement");
        self.expect_kind(
            TokenKind::LParen,
            "Expected `(` after effect operation name",
        );
        let mut args = Vec::new();
        if !self.check_kind(TokenKind::RParen) {
            loop {
                args.push(self.parse_expr(0));
                if self.consume_kind(TokenKind::Comma).is_none() {
                    break;
                }
            }
        }
        let end = self.expect_kind(
            TokenKind::RParen,
            "Expected `)` after effect operation arguments",
        );
        Stmt::Perform {
            binding,
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
            if self.check_kind(TokenKind::Dot) {
                self.bump();
                let field_span = self.current_span();
                let field = self.expect_identifier("Expected field name after `.`");
                let span = span_join(lhs.span, field_span);
                lhs = Expr {
                    kind: ExprKind::Field {
                        base: Box::new(lhs),
                        field,
                    },
                    span,
                };
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
            TokenKind::Keyword(Keyword::Match) => self.parse_match_expr(),
            TokenKind::Keyword(Keyword::Handle) => self.parse_handle_expr(),
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

    fn parse_match_expr(&mut self) -> Expr {
        let start = self.expect_keyword(Keyword::Match).span;
        let scrutinee = self.parse_expr(0);
        self.expect_kind(TokenKind::LBrace, "Expected `{` to open match clauses");

        let mut clauses = Vec::new();
        let mut default = None;

        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            self.consume_kind(TokenKind::Pipe);
            let clause_start = self.current_span();

            if self.consume_keyword(Keyword::Else).is_some() {
                self.expect_kind(TokenKind::FatArrow, "Expected `=>` after `else` in match");
                let body = self.parse_handler_clause_body();
                if default.is_some() {
                    self.diagnostics.error(
                        "PARSE_DUP_MATCH_DEFAULT",
                        "Duplicate default (`else` or `_`) match clause",
                        clause_start,
                    );
                } else {
                    default = Some(body);
                }
                self.consume_kind(TokenKind::Comma);
                self.consume_kind(TokenKind::Semi);
                continue;
            }

            let tag = self.expect_identifier("Expected match variant or `_`");
            if tag == self.wildcard_symbol {
                self.expect_kind(TokenKind::FatArrow, "Expected `=>` after `_` in match");
                let body = self.parse_handler_clause_body();
                if default.is_some() {
                    self.diagnostics.error(
                        "PARSE_DUP_MATCH_DEFAULT",
                        "Duplicate default (`else` or `_`) match clause",
                        clause_start,
                    );
                } else {
                    default = Some(body);
                }
                self.consume_kind(TokenKind::Comma);
                self.consume_kind(TokenKind::Semi);
                continue;
            }

            let mut binders = Vec::new();
            if self.consume_kind(TokenKind::LParen).is_some() {
                if !self.check_kind(TokenKind::RParen) {
                    loop {
                        binders.push(self.expect_identifier("Expected match arm binder"));
                        if self.consume_kind(TokenKind::Comma).is_none() {
                            break;
                        }
                    }
                }
                self.expect_kind(TokenKind::RParen, "Expected `)` after match arm binders");
            }

            self.expect_kind(TokenKind::FatArrow, "Expected `=>` after match arm head");
            let body = self.parse_handler_clause_body();
            clauses.push(MatchClause {
                tag,
                binders,
                span: span_join(clause_start, body.span),
                body,
            });

            self.consume_kind(TokenKind::Comma);
            self.consume_kind(TokenKind::Semi);
        }

        let end = self.expect_kind(TokenKind::RBrace, "Expected `}` to close match body");
        Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                clauses,
                default,
            },
            span: span_join(start, end.span),
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

    fn parse_handle_expr(&mut self) -> Expr {
        let start = self.expect_keyword(Keyword::Handle).span;
        let body = self.parse_expr(0);
        self.expect_keyword(Keyword::With);
        let effect = self.expect_identifier("Expected effect name after `with`");
        self.expect_kind(TokenKind::LBrace, "Expected `{` to open handler clauses");

        let mut clauses = Vec::new();
        while !self.check_kind(TokenKind::RBrace) && !self.at_eof() {
            self.consume_kind(TokenKind::Pipe);
            let clause_start = self.current_span();
            let operation = self.expect_identifier("Expected handler operation name");
            self.expect_kind(TokenKind::LParen, "Expected `(` after handler operation");
            let mut params = Vec::new();
            if !self.check_kind(TokenKind::RParen) {
                loop {
                    params.push(self.expect_identifier("Expected handler clause parameter"));
                    if self.consume_kind(TokenKind::Comma).is_none() {
                        break;
                    }
                }
            }
            self.expect_kind(
                TokenKind::RParen,
                "Expected `)` after handler clause parameters",
            );
            self.expect_kind(
                TokenKind::FatArrow,
                "Expected `=>` after handler clause head",
            );
            let clause_body = self.parse_handler_clause_body();
            let clause_span = span_join(clause_start, clause_body.span);
            clauses.push(HandleClause {
                operation,
                params,
                body: clause_body,
                span: clause_span,
            });
            self.consume_kind(TokenKind::Comma);
            self.consume_kind(TokenKind::Semi);
        }

        let end = self.expect_kind(TokenKind::RBrace, "Expected `}` to close handler body");
        Expr {
            kind: ExprKind::Handle {
                body: Box::new(body),
                effect,
                clauses,
            },
            span: span_join(start, end.span),
        }
    }

    fn parse_handler_clause_body(&mut self) -> BlockExpr {
        if self.check_kind(TokenKind::LBrace) {
            return self.parse_block();
        }

        let expr = self.parse_expr(0);
        BlockExpr {
            statements: Vec::new(),
            span: expr.span,
            tail: Some(Box::new(expr)),
        }
    }

    fn peek_binop(&self) -> Option<(BinOp, u8)> {
        map_binops!(
            self.current().kind,
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
            TokenKind::OrOr => BinOp::Or
        )
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

macro_rules! define_builtin_type_symbols {
    ($($field:ident => $name:literal => $variant:ident),* $(,)?) => {
        #[derive(Clone, Copy)]
        struct BuiltinTypeSymbols {
            $($field: SymbolId,)*
        }

        impl BuiltinTypeSymbols {
            fn intern(interner: &mut Interner) -> Self {
                Self {
                    $($field: interner.intern($name),)*
                }
            }

            fn resolve(self, symbol: SymbolId) -> Option<BuiltinType> {
                match symbol {
                    $(sym if sym == self.$field => Some(BuiltinType::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

define_builtin_type_symbols! {
    bool_ => "Bool" => Bool,
    int => "Int" => Int,
    float => "Float" => Float,
    char_ => "Char" => Char,
    string => "String" => String,
}

const fn effect_hint(
    capability: EffectCapabilityHint,
    discardable: bool,
    commutative: bool,
    opaque_for_staging: bool,
    ct_only: bool,
) -> EffectPropertyHint {
    EffectPropertyHint {
        capability,
        discardable,
        commutative,
        opaque_for_staging,
        ct_only,
    }
}

macro_rules! define_builtin_effect_symbols {
    ($($field:ident => $name:literal => $hint:expr),* $(,)?) => {
        #[derive(Clone, Copy)]
        struct BuiltinEffectSymbols {
            $($field: SymbolId,)*
        }

        impl BuiltinEffectSymbols {
            fn intern(interner: &mut Interner) -> Self {
                Self {
                    $($field: interner.intern($name),)*
                }
            }

            fn classify(self, symbol: SymbolId) -> EffectPropertyHint {
                match symbol {
                    $(sym if sym == self.$field => $hint,)*
                    _ => EffectPropertyHint::default(),
                }
            }
        }
    };
}

define_builtin_effect_symbols! {
    pure => "Pure" => effect_hint(EffectCapabilityHint::Pure, true, true, false, false),
    diverge => "Diverge" => effect_hint(EffectCapabilityHint::Diverge, false, false, false, false),
    alloc => "Alloc" => effect_hint(EffectCapabilityHint::Alloc, false, false, false, false),
    state => "State" => effect_hint(EffectCapabilityHint::LocalState, false, false, false, false),
    local_state => "LocalState" => effect_hint(EffectCapabilityHint::LocalState, false, false, false, false),
    shared_state => "SharedState" => effect_hint(EffectCapabilityHint::SharedState, false, false, true, false),
    atomic_state => "AtomicState" => effect_hint(EffectCapabilityHint::SharedState, false, false, true, false),
    atomic => "Atomic" => effect_hint(EffectCapabilityHint::SharedState, false, false, true, false),
    io => "IO" => effect_hint(EffectCapabilityHint::Io, false, false, true, false),
    console => "Console" => effect_hint(EffectCapabilityHint::Io, false, false, true, false),
    system => "System" => effect_hint(EffectCapabilityHint::Io, false, false, true, false),
    ffi => "FFI" => effect_hint(EffectCapabilityHint::Ffi, false, false, true, false),
    comptime_read_files => "ComptimeReadFiles" => effect_hint(EffectCapabilityHint::LocalState, false, false, false, true),
    build_fs => "BuildFS" => effect_hint(EffectCapabilityHint::LocalState, false, false, false, true),
}
