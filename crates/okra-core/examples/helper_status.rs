//! Ask the elevated NTFS helper for its health, from an ordinary (unelevated) process, exactly
//! the way the launcher does. Exit code 0 means the helper is serving searches.
//!
//!     cargo run -p okra-core --example helper_status
fn main() {
    let status = okra_core::providers::files::ntfs_helper_status();
    println!("{}", status.describe());
    std::process::exit(if status.is_serving() { 0 } else { 1 });
}
