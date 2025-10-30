use crate::common::diagnostics::ErrorNode;
use crate::common::ids::{
    EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, TypeId, VarId,
};
use crate::common::span::Span;
use crate::sema::effect::SortedEffectRow;

#[derive(Clone, PartialEq, Debug)]
pub enum Literal {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    String(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageDirective {
    Comptime,
    Runtime,
}

#[derive(Clone, Debug)]
pub struct ExprNode {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Var(VarId),
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: ExprId,
    },
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    PureCall {
        callee: FuncId,
        args: Vec<ExprId>,
    },
    MakeStruct {
        ty: TypeId,
        fields: Vec<ExprId>,
    },
    MakeEnum {
        ty: TypeId,
        variant: SymbolId,
        fields: Vec<ExprId>,
    },
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct StmtNode {
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Return(ExprId),
    Let {
        binding: VarId,
        value: ExprId,
        next: StmtId,
    },
    Val {
        binding: VarId,
        value: StmtId,
        next: StmtId,
    },
    Call {
        result: VarId,
        callee: FuncId,
        args: Vec<ExprId>,
        effects: SortedEffectRow,
        next: StmtId,
    },
    If {
        cond: ExprId,
        then_branch: StmtId,
        else_branch: StmtId,
    },
    Match {
        scrutinee: ExprId,
        arms: Vec<MatchArm>,
        default: Option<StmtId>,
    },
    Perform {
        result: Option<VarId>,
        effect: EffectLabelId,
        operation: SymbolId,
        args: Vec<ExprId>,
        next: StmtId,
    },
    Handle {
        handler: HandlerId,
        body: StmtId,
        next: Option<StmtId>,
    },
    Stage {
        stage: StageDirective,
        body: StmtId,
        next: Option<StmtId>,
    },
    Hole {
        ty: TypeId,
    },
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub tag: SymbolId,
    pub binders: Vec<VarId>,
    pub body: StmtId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HandlerClause {
    pub operation: SymbolId,
    pub params: Vec<VarId>,
    pub resume_param: Option<VarId>,
    pub body: StmtId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HandlerDef {
    pub effect: EffectLabelId,
    pub return_param: VarId,
    pub return_body: StmtId,
    pub clauses: Vec<HandlerClause>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FunctionDecl {
    pub name: SymbolId,
    pub params: Vec<VarId>,
    pub param_types: Vec<TypeId>,
    pub return_type: TypeId,
    pub declared_effects: SortedEffectRow,
    pub body: StmtId,
    pub ct_only: bool,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct CoreProgram {
    exprs: Vec<ExprNode>,
    stmts: Vec<StmtNode>,
    handlers: Vec<HandlerDef>,
    functions: Vec<FunctionDecl>,
    entrypoints: Vec<FuncId>,
}

impl CoreProgram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_expr(&mut self, node: ExprNode) -> ExprId {
        let id = ExprId::new(self.exprs.len());
        self.exprs.push(node);
        id
    }

    pub fn push_stmt(&mut self, node: StmtNode) -> StmtId {
        let id = StmtId::new(self.stmts.len());
        self.stmts.push(node);
        id
    }

    pub fn add_handler(&mut self, handler: HandlerDef) -> HandlerId {
        let id = HandlerId::new(self.handlers.len());
        self.handlers.push(handler);
        id
    }

    pub fn add_function(&mut self, function: FunctionDecl) -> FuncId {
        let id = FuncId::new(self.functions.len());
        self.functions.push(function);
        id
    }

    pub fn set_entrypoints(&mut self, entrypoints: impl IntoIterator<Item = FuncId>) {
        self.entrypoints.clear();
        self.entrypoints.extend(entrypoints);
    }

    pub fn exprs(&self) -> &[ExprNode] {
        &self.exprs
    }

    pub fn stmts(&self) -> &[StmtNode] {
        &self.stmts
    }

    pub fn handlers(&self) -> &[HandlerDef] {
        &self.handlers
    }

    pub fn functions(&self) -> &[FunctionDecl] {
        &self.functions
    }

    pub fn entrypoints(&self) -> &[FuncId] {
        &self.entrypoints
    }

    pub fn expr(&self, id: ExprId) -> Option<&ExprNode> {
        self.exprs.get(id.index())
    }

    pub fn stmt(&self, id: StmtId) -> Option<&StmtNode> {
        self.stmts.get(id.index())
    }

    pub fn function(&self, id: FuncId) -> Option<&FunctionDecl> {
        self.functions.get(id.index())
    }

    pub fn function_mut(&mut self, id: FuncId) -> Option<&mut FunctionDecl> {
        self.functions.get_mut(id.index())
    }
}
