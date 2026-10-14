use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

fn run() -> io::Result<()> {
    let mut args = app_caps::args_os().skip(1);
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
    let count = pdf_toolkit_write::fixtures::write_fixtures(&destination)?;
    println!("wrote {count} fixtures");
    Ok(())
}

#[path = "app_caps/mod.rs"]
mod app_caps;

fn main() -> ExitCode {
    let _caps = app_caps::start([2147483648, 300, 600, 256, 2, 32, 268435456, 10000, 10000]);
    app_caps::stage("pdf-write/src/bin/pdf-fixtures.rs: fixture generation or round trip");
    app_caps::turn("write PDF fixtures");
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pdf-fixtures: {error}");
            ExitCode::FAILURE
        }
    }
}
