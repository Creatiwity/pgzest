fn main() {
    pgweave_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgweave : {e}"));
}
