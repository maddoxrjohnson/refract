//! `refract-agent`: speaks newline-delimited JSON on stdin and stdout until `shutdown`
//! or the end of input. Logs go to stderr. `refract-agent --version` prints
//! [`refract_agent::VERSION_LINE`], which the app checks before reinstalling it.

use std::io::{self, BufReader};
use std::process::ExitCode;

use refract_agent::config::Config;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{}", refract_agent::VERSION_LINE);
        return ExitCode::SUCCESS;
    }
    let config = Config::load();
    let input = BufReader::new(io::stdin());
    let out = io::LineWriter::new(io::stdout().lock());
    match refract_agent::server::run(config, input, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("refract-agent: {e}");
            ExitCode::FAILURE
        }
    }
}
