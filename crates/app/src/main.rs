//! Binary entry point: `removarr [serve | healthcheck]`.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(removarr::cli::main(&args))
}
