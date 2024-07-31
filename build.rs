use std::process::{Command, ExitStatus, Output};
use std::time::SystemTime;

#[cfg(not(target_os = "windows"))]
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "windows")]
use std::os::windows::process::ExitStatusExt;

fn main() {
	println!("cargo:rerun-if-changed=src/");
	let output = String::from_utf8(
		Command::new("git")
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap_or(Output {
				stdout: vec![],
				stderr: vec![],
				status: ExitStatus::from_raw(0),
			})
			.stdout,
	)
	.unwrap_or_default();
	let git_hash = if output == String::default() { "dev".into() } else { output };
	let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("get millis error");
	let build_time = now.as_millis();
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
	println!("cargo:rustc-env=BUILD_TIME={build_time}");
}
