//! Writes the signed house fixtures (author-signed, author-signed-twice and
//! author-signed-edited) with their expectations into a directory.
//!
//! Usage: pdf-sign-fixtures DIR

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../sign_fixtures.rs"]
mod fixtures;

#[path = "../../../pdf-apps/src/app_caps.rs"]
mod app_caps;

fn main() -> ExitCode {
    let _caps = app_caps::start([2147483648, 300, 600, 256, 2, 32, 268435456, 10000, 10000]);
    app_caps::stage("pdf-edit/src/bin/pdf-sign-fixtures.rs: fixture generation or round trip");
    let Some(directory) = app_caps::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: pdf-sign-fixtures DIR");
        return ExitCode::from(2);
    };
    let result = (|| -> Result<usize, String> {
        let built = fixtures::build()?;
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        for fixture in &built {
            fs::write(
                directory.join(format!("{}.pdf", fixture.name)),
                &fixture.pdf,
            )
            .map_err(|error| error.to_string())?;
            fs::write(
                directory.join(format!("{}.expect", fixture.name)),
                &fixture.expected,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(built.len())
    })();
    match result {
        Ok(count) => {
            println!("wrote {count} signed fixtures");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("pdf-sign-fixtures: {error}");
            ExitCode::FAILURE
        }
    }
}
