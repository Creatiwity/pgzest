fn main() {
    pgzest_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgzest: {e}"));
}
