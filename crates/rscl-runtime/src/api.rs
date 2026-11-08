//! The table of runtime functions that generated code may call. One row per function:
//! symbol name, address (for the JIT), and the C signature (for Cranelift imports).
//! Adding a row without a function is a compile error, so the table is exhaustive.
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    /// Pointer-sized integer: values, pointers, i64 counts.
    I64,
    F64,
    I32,
}

pub struct RtFnDesc {
    pub name: &'static str,
    pub addr: *const u8,
    pub params: &'static [Ty],
    pub ret: Option<Ty>,
}
// SAFETY: addresses of functions.
unsafe impl Sync for RtFnDesc {}

macro_rules! runtime_fns {
    ($( $variant:ident = $name:literal => $path:path : ( $($p:ident),* ) $(-> $r:ident)? ; )*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub enum RtFn { $($variant),* }

        impl RtFn {
            pub const ALL: &'static [RtFn] = &[$(RtFn::$variant),*];

            #[allow(unknown_lints, function_casts_as_integer)]
            pub fn desc(self) -> RtFnDesc {
                match self {
                    $(RtFn::$variant => RtFnDesc {
                        name: $name,
                        addr: $path as usize as *const u8,
                        params: &[$(Ty::$p),*],
                        ret: None $(.or(Some(Ty::$r)))?,
                    },)*
                }
            }

            pub fn name(self) -> &'static str {
                self.desc().name
            }
        }
    };
}

runtime_fns! {
    Retain        = "rt_retain"         => crate::ffi::rt_retain : (I64);
    Release       = "rt_release"        => crate::ffi::rt_release : (I64);
    Throw         = "rt_throw"          => crate::exc::rt_throw : (I64) -> I64;
    TakeException = "rt_take_exception" => crate::exc::rt_take_exception : () -> I64;
    InternVar     = "rt_intern_var"     => crate::ns::rt_intern_var : (I64, I64, I64, I64) -> I64;
    VarRoot       = "rt_var_root"       => crate::ns::rt_var_root : (I64) -> I64;
    VarBindRoot   = "rt_var_bind_root"  => crate::ns::rt_var_bind_root : (I64, I64) -> I64;
    VarSetFlag    = "rt_var_set_flag"   => crate::ns::rt_var_set_flag : (I64, I64, I64);
    BoxDouble     = "rt_box_double"     => crate::ffi::rt_box_double : (F64) -> I64;
    BoxLong       = "rt_box_long"       => crate::ffi::rt_box_long : (I64) -> I64;
    StringNew     = "rt_string_new"     => crate::ffi::rt_string_new : (I64, I64) -> I64;
    Keyword       = "rt_keyword"        => crate::ffi::rt_keyword : (I64, I64, I64, I64) -> I64;
    Symbol        = "rt_symbol"         => crate::ffi::rt_symbol : (I64, I64, I64, I64) -> I64;
    ReadConst     = "rt_read_const"     => crate::ffi::rt_read_const : (I64, I64) -> I64;
    FnProtoNew    = "rt_fnproto_new"    => crate::ffi::rt_fnproto_new : (I64, I64, I64, I64, I64) -> I64;
    FnNew         = "rt_fn_new"         => crate::ffi::rt_fn_new : (I64, I64) -> I64;
    FnSetCapture  = "rt_fn_set_capture" => crate::ffi::rt_fn_set_capture : (I64, I64, I64);
    ThrowArity    = "rt_throw_arity"    => crate::ffi::rt_throw_arity : (I64, I64) -> I64;
    Invoke        = "rt_invoke"         => crate::ffi::rt_invoke : (I64, I64, I64) -> I64;
    ListFromSlice = "rt_list_from_slice" => crate::ffi::rt_list_from_slice : (I64, I64) -> I64;
    VectorFromSlice = "rt_vector_from_slice" => crate::ffi::rt_vector_from_slice : (I64, I64) -> I64;
    MapFromSlice  = "rt_map_from_slice" => crate::ffi::rt_map_from_slice : (I64, I64) -> I64;
    SetFromSlice  = "rt_set_from_slice" => crate::ffi::rt_set_from_slice : (I64, I64) -> I64;
    MakeImmortal  = "rt_make_immortal"  => crate::ffi::rt_make_immortal : (I64);
    PrStr         = "rt_pr_str"         => crate::ffi::rt_pr_str : (I64) -> I64;
    Prn           = "rt_prn"            => crate::ffi::rt_prn : (I64);
    ReportUncaught = "rt_report_uncaught" => crate::ffi::rt_report_uncaught : () -> I32;
    RunMain       = "rt_run_main"       => crate::ffi::rt_run_main : (I64) -> I32;
    StackCheck    = "rt_stack_check"    => crate::native::rt_stack_check : (I64) -> I64;
    Interrupted   = "rt_interrupted"    => crate::native::rt_interrupted : () -> I64;
    NativeSymbol  = "rt_native_symbol"  => crate::native::rt_native_symbol : (I64, I64) -> I64;
    ThrowUnresolvedNative = "rt_throw_unresolved_native" => crate::native::rt_throw_unresolved_native : (I64, I64) -> I64;
    ToLong        = "rt_to_long"        => crate::native::rt_to_long : (I64, I64) -> I64;
    ToDouble      = "rt_to_double"      => crate::native::rt_to_double : (I64, I64) -> I64;
    ToCstr        = "rt_to_cstr"        => crate::native::rt_to_cstr : (I64, I64) -> I64;
    StringFromCstr = "rt_string_from_cstr" => crate::native::rt_string_from_cstr : (I64) -> I64;
    ExceptionInstance = "rt_exception_instance" => crate::native::rt_exception_instance : (I64, I64, I64) -> I64;
    RegisterNsLoader = "rt_register_ns_loader" => crate::ns::rt_register_ns_loader : (I64, I64, I64);
    CurrentNs     = "rt_current_ns"     => crate::ns::rt_current_ns : () -> I64;
    RegisterCode  = "rt_register_code"  => crate::trace::rt_register_code : (I64, I64, I64, I64);
    RuntimeInit   = "rt_runtime_init"   => crate::entry::rt_runtime_init : (I32, I64);
    Main          = "rt_main"           => crate::entry::rt_main : (I32, I64, I64) -> I32;
    Shutdown      = "rt_shutdown"       => crate::entry::rt_shutdown : ();
}

/// Immediate encodings the compiler needs (kept here so there is one source of truth).
pub mod consts {
    pub use crate::value::{EXC, FALSE, NIL, TRUE, UNBOUND};
    pub const KIND_FN: u32 = crate::obj::Kind::Fn as u32;
    pub const OFF_KIND: i32 = 8;
    pub const OFF_PROTO: i32 = crate::obj::func::OFF_PROTO;
    pub const OFF_NCAPTURES: i32 = crate::obj::func::OFF_NCAPTURES;
    pub const OFF_CAPTURES: i32 = crate::obj::func::OFF_CAPTURES;
    pub const PROTO_OFF_ENTRY: i32 = crate::obj::func::PROTO_OFF_ENTRY;
}

#[allow(dead_code)]
fn _value_is_word() {
    const _: () = assert!(std::mem::size_of::<Value>() == 8);
}
