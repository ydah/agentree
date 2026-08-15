use std::ffi::OsString;

use agentree::app::Application;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let exit = match Application::dispatch(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", error.render());
            error.exit_code()
        }
    };
    std::process::exit(exit);
}
