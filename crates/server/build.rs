fn main() {
    if std::env::var_os("CARGO_FEATURE_EMBED_UI").is_none() {
        return;
    }
    println!("cargo:rerun-if-changed=../../ui/dist/index.html");
    let index = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist/index.html");
    if !index.is_file() {
        panic!(
            "embed-ui requires ui/dist (missing {}).\nbuild ui first: cd ui && npm ci && npm run build",
            index.display()
        );
    }
}
