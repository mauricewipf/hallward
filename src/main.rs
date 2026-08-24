fn main() {
    if let Err(err) = hallward::run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
