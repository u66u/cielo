use cielo_base::ids::{ResumptionId, StmtId, VarId};

/// What a Core statement graph's `Return` answers to while a handler is being
/// erased.
///
/// A `Return` is not always the end of the handled body: the value graph of a
/// `Val` returns *into* the `Val`, and the rest of the enclosing body is still
/// to come. Naming that pending tail is what lets a `resume` under a nested
/// block continue with the whole handled body instead of stopping at the
/// block's edge (CIELO-66).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Answer<'a> {
    /// The graph's value is the value of the linear statement it lowers to.
    Yield,
    /// End of the handled body: the handler's return clause consumes the value.
    /// Only the outermost graph of a handled body answers this way -- the
    /// return clause runs once, not once per nested block.
    HandlerReturn,
    /// The graph is a `Val`'s value and something in it splices a continuation,
    /// so the rest of the enclosing graph is inlined at each return rather than
    /// joined after it. A `Val` join is unreachable from a spliced `resume`,
    /// which is the whole reason this frame exists.
    Bind {
        binding: VarId,
        next: StmtId,
        /// The clause context in force at the `Val`, which is not the one in
        /// force at the return that lands here: a `resume` re-enters this frame
        /// from inside a clause spliced several levels down.
        resume_ctx: Option<&'a ResumeContext<'a>>,
        outer: &'a Answer<'a>,
    },
}

/// A clause context is a stack, not a single frame. A clause that performs
/// another effect before resuming has that effect's clause spliced *inside* it,
/// and the inner clause's `resume` splices back the code that still has to
/// reach the outer `resume`. Dropping `outer` on the way in loses the only
/// record of which continuation that outer `resume` names (CIELO-54).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ResumeContext<'a> {
    pub(super) resume_var: VarId,
    pub(super) perform_result: Option<VarId>,
    pub(super) continuation: StmtId,
    /// What `continuation` answers to. The continuation is the perform site's,
    /// so it answers where the perform site did, not where the `resume` that
    /// splices it sits (CIELO-66).
    pub(super) answer: &'a Answer<'a>,
    pub(super) clause_convention: ClauseConvention,
    pub(super) policy: ClausePolicy,
    /// The shared resumption every `resume` in this clause enters. `Some` for
    /// exactly [`ClausePolicy::Defunctionalise`]; naming it here rather than
    /// taking the innermost open one is what keeps an *outer* clause's `resume`
    /// from being captured by an inner clause spliced around it (CIELO-54).
    pub(super) resumption: Option<ResumptionId>,
    /// The context in force at the perform site this clause answers.
    pub(super) outer: Option<&'a ResumeContext<'a>>,
}

/// What lowering does with one handler clause: the single decision erasure and
/// dispatch are both read off.
///
/// It used to be two decisions in two places — `resume_strategy` at the perform
/// site chose how to erase, `residual_clause_blocker` at the handle site chose
/// whether to dispatch — with nothing relating the answers. They are now the
/// arms of one enum, picked by [`super::analysis::clause_policy`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ClausePolicy {
    /// Splice the clause and the continuation together at the perform site.
    Erase(ResumeStrategy),
    /// Splice the clause once and enter one shared copy of the continuation
    /// from every `resume`, leaving it through an integer dispatch back to the
    /// code after the site that entered it (CIELO-39).
    Defunctionalise,
    /// Leave the clause to the runtime dispatcher as a function whose value is
    /// the resumption argument (CIELO-19).
    Dispatch,
}

/// Where the performs a clause answers sit relative to the handle site, which
/// is what decides whether erasure is on the table at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PerformReach {
    /// Lexically inside the handled body. Splicing the clause in is what
    /// discharges it, so no clause shape can be refused here.
    Lexical,
    /// Behind a call, or left over after inlining. Nothing but a runtime
    /// dispatch reaches it.
    Runtime,
}

/// How an erased clause's `resume` sites reach the handled continuation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResumeStrategy {
    /// Re-lower the continuation at every site. A clause with `k` sites over a
    /// body with `n` performs then expands as `k^n`.
    Inline,
    /// Every site returns into one `Val` join that binds the continuation once
    /// (Maurer et al., "Compiling without Continuations", PLDI 2017).
    Join,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ClauseConvention {
    Pure,
    Direct,
    Control,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResumeUseBound {
    Zero,
    One,
    Many,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResumeQualifier {
    Abortive,
    Affine,
    Linear,
    Multi,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ClauseResumeAnalysis {
    pub(super) qualifier: ResumeQualifier,
    pub(super) min_uses: ResumeUseBound,
    pub(super) max_uses: ResumeUseBound,
    pub(super) tail_resumptive: bool,
    /// Syntactic `resume` occurrences, unlike `min_uses`/`max_uses`, which
    /// bound how many run on one path.
    pub(super) sites: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ResumeUseRange {
    pub(super) min: ResumeUseBound,
    pub(super) max: ResumeUseBound,
}

impl ResumeUseBound {
    pub(super) fn plus(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Many, _) | (_, Many) => Many,
            (One, One) => Many,
            (One, Zero) | (Zero, One) => One,
            (Zero, Zero) => Zero,
        }
    }

    pub(super) fn max(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Many, _) | (_, Many) => Many,
            (One, _) | (_, One) => One,
            (Zero, Zero) => Zero,
        }
    }

    pub(super) fn min(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Zero, _) | (_, Zero) => Zero,
            (One, _) | (_, One) => One,
            (Many, Many) => Many,
        }
    }

    pub(super) fn is_many(self) -> bool {
        matches!(self, Self::Many)
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::One => "one",
            Self::Many => "many",
        }
    }
}

impl ResumeQualifier {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Abortive => "Abortive",
            Self::Affine => "Affine",
            Self::Linear => "Linear",
            Self::Multi => "Multi",
        }
    }
}

impl ClauseConvention {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "Pure",
            Self::Direct => "Direct",
            Self::Control => "Control",
        }
    }
}

impl ClausePolicy {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Erase(ResumeStrategy::Inline) => "erase, continuation re-lowered per resume",
            Self::Erase(ResumeStrategy::Join) => "erase, resume sites merged into one join",
            Self::Defunctionalise => "erase, one shared continuation with an integer dispatch",
            Self::Dispatch => "runtime clause table",
        }
    }
}

impl ResumeUseRange {
    pub(super) fn zero() -> Self {
        Self {
            min: ResumeUseBound::Zero,
            max: ResumeUseBound::Zero,
        }
    }

    pub(super) fn many() -> Self {
        Self {
            min: ResumeUseBound::Zero,
            max: ResumeUseBound::Many,
        }
    }

    pub(super) fn plus(self, other: Self) -> Self {
        Self {
            min: self.min.plus(other.min),
            max: self.max.plus(other.max),
        }
    }

    pub(super) fn join(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}
