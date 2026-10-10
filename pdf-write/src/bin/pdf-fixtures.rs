use std::env;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let destination = args.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "usage: pdf-fixtures OUT_DIR")
    })?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: pdf-fixtures OUT_DIR",
        ));
    }
    let destination = PathBuf::from(destination);
    let count = pdf_write::fixtures::write_fixtures(&destination)?;
    println!("wrote {count} fixtures");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdf-fixtures: {error}");
            ExitCode::FAILURE
        }
    }
}

