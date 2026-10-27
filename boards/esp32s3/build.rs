//! Link with esp-hal's memory map.

fn main() {
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}
