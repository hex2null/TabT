// The product identity is baked in at compile time (see src/branding.rs), so the crate has to be
// rebuilt when the Makefile switches between the development and the release identity.
fn main() {
    println!("cargo:rerun-if-env-changed=TABT_APP_NAME");
    println!("cargo:rerun-if-env-changed=TABT_CONFIG_DIR");
}
