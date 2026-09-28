use std::io::Read;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut pretty = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--pretty" | "-p" => pretty = true,
            "--help" | "-h" => {
                print!("{}", layer_merge::USAGE);
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("layer-merge: unknown argument '{other}'\n\n{}", layer_merge::USAGE);
                return ExitCode::from(2);
            }
        }
    }

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("layer-merge: failed to read stdin: {e}");
        return ExitCode::from(2);
    }

    match layer_merge::run(&buf) {
        Ok(output) => {
            println!("{}", layer_merge::ok_json(&output, pretty));
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("{}", layer_merge::err_json(&err, pretty));
            ExitCode::from(err.exit_code())
        }
    }
}
