fn main() {
    if let Err(e) = zipkirei::cli::run_from_env() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
