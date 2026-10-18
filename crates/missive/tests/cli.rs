use std::process::Command;

#[test]
fn missive_without_a_command_shows_its_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_missive"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage: missive"), "{stderr}");
}
