fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    strata_cli::run(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
}
