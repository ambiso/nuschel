// SPDX-License-Identifier: EUPL-1.2
//! Size baseline: std only, comparable work shape (no MLS).
fn main() {
    let input: Vec<String> = std::env::args().collect();
    println!("{}", input.len());
}
