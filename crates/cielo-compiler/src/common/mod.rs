//! Compatibility namespace for code that is still being migrated.
//!
//! Definitions live in `cielo-base`; this module must not grow new shared
//! compiler state.

pub mod densemap {
    pub use cielo_base::densemap::*;
}

pub mod diagnostics {
    pub use cielo_base::diagnostics::*;
}

pub mod fixpoint {
    pub use cielo_base::fixpoint::*;
}

pub mod ids {
    pub use cielo_base::ids::*;
}

pub mod reporting {
    pub use cielo_base::reporting::*;
}

pub mod span {
    pub use cielo_base::span::*;
}

pub mod symbols {
    pub use cielo_base::symbols::*;
}

pub mod gc {
    pub use cielo_memory::config::*;
}
