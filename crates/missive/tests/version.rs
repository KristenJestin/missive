use std::process::Command;

// `Cargo.toml` stays at 0.0.0: a release build names its version in MISSIVE_VERSION (the tag it
// is published under), a local build says it is a development build.
#[test]
fn version_is_the_tag_of_the_build_and_a_local_build_says_it_is_a_development_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_missive"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let expected = option_env!("MISSIVE_VERSION").unwrap_or("development build");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("missive {expected}\n")
    );
}
