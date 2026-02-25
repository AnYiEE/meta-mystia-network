fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS")
        .map(|s| s == "windows")
        .unwrap_or(false)
    {
        thunk::thunk();
    }
}
