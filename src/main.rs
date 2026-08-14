fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match brewsoak::dispatch(&args, &brewsoak::RealWorld::new()) {
        Ok(brewsoak::Dispatch::Exit(c)) => c,
        Ok(brewsoak::Dispatch::Exec(bin, argv)) => brewsoak::brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoak: {e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}
