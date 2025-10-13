use std::process::Command;

fn main() {
    // get git commit
    let command = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let commit = match command {
        Ok(output) => String::from_utf8(output.stdout).unwrap(),
        // if error, e.g. build from source without git repo, just show empty string
        Err(_) => "".to_string(),
    };
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
}
