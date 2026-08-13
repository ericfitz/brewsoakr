fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match brewsoakr::dispatch(&args, &brewsoakr::RealWorld::new()) {
        Ok(brewsoakr::Dispatch::Exit(c)) => c,
        Ok(brewsoakr::Dispatch::Exec(bin, argv)) => brewsoakr::brew::exec(&bin, &argv),
        Err(e) => {
            eprintln!("brewsoakr: {e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}
