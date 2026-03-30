//! Semantic integration compatibility namespace.

pub mod effect;

pub mod ownership {
    pub use cielo_sema::ownership::*;
}

pub mod ty {
    pub use cielo_sema::ty::*;
}

pub mod typecheck;
