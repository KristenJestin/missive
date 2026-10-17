use std::process::Command;

#[test]
fn missive_still_builds_and_says_it_does_nothing_yet() {
    let output = Command::new(env!("CARGO_BIN_EXE_missive"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("nothing here yet"), "{stderr}");
}
