fn main() {
    if let Err(err) = hallward::run() {
        if err.to_string() == "indexing cancelled" {
            std::process::exit(130);
        }
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
