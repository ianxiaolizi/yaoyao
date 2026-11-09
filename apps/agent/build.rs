fn main() {
    // The Windows link step defaults a GUI/console executable to a 1 MiB stack.
    // A turn can validate several tool calls while the model response and loop
    // state are still live; schema validation then needs more than that reserve.
    // Keep the extra space on the final binary rather than spawning a helper
    // thread that would change process and console semantics.
    println!("cargo:rustc-link-arg-bins=/STACK:8388608");
}
