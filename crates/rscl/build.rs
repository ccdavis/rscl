fn main() {
    // Export the host's symbols so `defnative` can resolve libc/libm and runtime
    // functions by name through dlsym(RTLD_DEFAULT) in JIT mode.
    println!("cargo:rustc-link-arg-bins=-rdynamic");
    // Keep libm linked even though Rust itself needs nothing from it, so the typed
    // FFI can resolve `cos`, `sqrt`, ... with dlsym in the JIT host.
    println!("cargo:rustc-link-arg-bins=-Wl,--no-as-needed");
    println!("cargo:rustc-link-arg-bins=-lm");
    println!("cargo:rerun-if-changed=build.rs");
}
