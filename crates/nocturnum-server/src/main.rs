//! The nocturnum binary, which serves the API and the embedded console on one port.

fn main() {
    println!("nocturnum {}", env!("CARGO_PKG_VERSION"));
}
