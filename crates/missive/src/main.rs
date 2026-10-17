// A release build names its version in MISSIVE_VERSION (the tag it is published under, with its
// `v`); Cargo.toml stays at 0.0.0. A local build has no such name and says what it is.
const VERSION: &str = match option_env!("MISSIVE_VERSION") {
    Some(version) => version,
    None => "development build",
};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("missive {VERSION}");
        return;
    }
    eprintln!("missive: nothing here yet (see the tracker)");
    std::process::exit(2);
}
