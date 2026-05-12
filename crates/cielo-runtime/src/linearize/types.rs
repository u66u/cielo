use cielo_base::ids::{StmtId, VarId};

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
    pub(super) clause_convention: ClauseConvention,
    pub(super) strategy: ResumeStrategy,
    /// The context in force at the perform site this clause answers.
    pub(super) outer: Option<&'a ResumeContext<'a>>,
}

/// How a clause's `resume` sites reach the handled continuation.
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
