fn main() {
    if let Err(err) = bzbd::run() {
        eprintln!("bzbd: {err:#}");
        std::process::exit(1);
    }
}
