use std::process::Command;

fn lucy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lucy"))
}

#[test]
fn help_is_successful_and_documents_public_commands() {
    let output = lucy().arg("--help").output().expect("lucy should run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help should be UTF-8");
    assert!(stdout.contains("Usage: lucy <COMMAND>"));
    assert!(stdout.contains("serve"));
    assert!(stdout.contains("validate"));
    assert!(!stdout.contains("__healthcheck"));
}

#[test]
fn version_is_successful_and_uses_package_version() {
    let output = lucy().arg("--version").output().expect("lucy should run");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("lucy {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_syntax_uses_the_documented_usage_exit_code() {
    let status = lucy()
        .args(["serve", "config/legacy.yaml"])
        .status()
        .expect("lucy should run");
    assert_eq!(status.code(), Some(2));
}
