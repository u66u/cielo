macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(u32);

        impl $name {
            pub const INVALID: Self = Self(u32::MAX);

            #[inline]
            pub fn new(index: usize) -> Self {
                assert!(index <= u32::MAX as usize);
                Self(index as u32)
            }

            #[inline]
            pub const fn from_u32(value: u32) -> Self {
                Self(value)
            }

            #[inline]
            pub const fn index(self) -> usize {
                self.0 as usize
            }

            #[inline]
            pub const fn as_u32(self) -> u32 {
                self.0
            }

            #[inline]
            pub const fn is_valid(self) -> bool {
                self.0 != u32::MAX
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::INVALID
            }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(index: usize) -> Self {
                Self::new(index)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(SourceId);
define_id!(SymbolId);
define_id!(TypeId);
define_id!(EffectLabelId);
define_id!(VarId);
define_id!(ExprId);
define_id!(StmtId);
define_id!(LinearExprId);
define_id!(LinearStmtId);
define_id!(LinearFuncId);
define_id!(ResumptionId);
define_id!(CfgExprId);
define_id!(CfgInstId);
define_id!(CfgBlockId);
define_id!(CfgValueId);
define_id!(CfgFuncId);
define_id!(CfgHandlerId);
define_id!(CfgRegionId);
define_id!(FuncId);
define_id!(StructId);
define_id!(EnumId);
define_id!(HandlerId);
define_id!(ConstId);
define_id!(DiagnosticId);
