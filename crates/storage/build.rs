//! Re-embeds the migrations when a file under `migrations/` changes.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
