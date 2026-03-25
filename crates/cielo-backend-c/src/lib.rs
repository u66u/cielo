//! Pure CFG-to-C rendering and the C runtime support header.

pub mod c_constants;
pub mod cfg_codegen;

// Compatibility namespaces used by the physically moved renderer. They can
// be removed as its imports are simplified.
pub mod common {
    pub mod ids {
        pub use cielo_base::ids::*;
    }

    pub mod symbols {
        pub use cielo_base::symbols::*;
    }
}

pub mod ir {
    pub use cielo_ir::{cfg, core, linear};
}

pub mod passes {
    pub use crate::c_constants;
}

pub mod pipeline {
    pub mod phases {
        pub use cielo_staging::pipeline::phases::*;
    }
}

pub const RUNTIME_HEADER: &str = include_str!("cielo_runtime.h");

pub fn capabilities() -> cielo_backend_api::BackendCapabilities {
    cielo_backend_api::BackendCapabilities::new([
        cielo_backend_api::RuntimeRequirement::ReferenceCounting,
    ])
}
