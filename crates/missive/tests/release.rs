//! The release scripts, run for real on a stand-in program: the archive `tools/package.sh` makes,
//! the `install.sh` inside it and the version of a beta. Nothing here touches the real `~/.local`,
//! the real configuration or the keyring: every install goes into a temporary prefix, with an
//! empty temporary home.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository root, found from the package folder so that it holds wherever the package sits.
fn root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    while !dir.join("tools/package.sh").exists() {
        assert!(dir.pop(), "tools/package.sh not found above the package");
    }
    dir
}

/// A temporary folder, removed when the test is over.
struct Work(PathBuf);

impl Work {
    fn new(test: &str) -> Work {
        let dir = std::env::temp_dir().join(format!("missive-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Work(dir)
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A program that says its version, as `cargo build --release` leaves one in a folder.
fn built(dir: &Path, tag: &str) {
    fs::create_dir_all(dir).unwrap();
    let program = dir.join("missive");
    fs::write(&program, format!("#!/bin/sh\necho \"missive {tag}\"\n")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Packs the stand-in as `tag` and unpacks the archive: the folder an install starts from.
fn unpacked(work: &Path, tag: &str) -> PathBuf {
    let binaries = work.join("target");
    built(&binaries, tag);
    let out = work.join("dist");
    let packed = run(Command::new(root().join("tools/package.sh"))
        .arg(tag)
        .arg(&binaries)
        .arg(&out));
    let archive = String::from_utf8(packed.stdout).unwrap().trim().to_string();
    assert_eq!(archive, format!("missive-{tag}-x86_64-linux.tar.gz"));
    let extracted = work.join("extracted");
    fs::create_dir(&extracted).unwrap();
    run(Command::new("tar")
        .arg("-xzf")
        .arg(out.join(&archive))
        .arg("-C")
        .arg(&extracted));
    extracted.join(archive.trim_end_matches(".tar.gz"))
}

/// The temporary data folder an install puts the desktop entry in (`XDG_DATA_HOME`).
fn data(work: &Path) -> PathBuf {
    work.join("data")
}

fn install(folder: &Path, work: &Path, prefix: &Path) -> Output {
    let home = work.join("home");
    fs::create_dir_all(&home).unwrap();
    run(Command::new("sh")
        .arg(folder.join("install.sh"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", &home)
        .env("XDG_DATA_HOME", data(work))
        .env("PREFIX", prefix))
}

#[test]
fn the_archive_holds_the_binary_and_the_install_script_with_its_checksum() {
    let work = Work::new("archive");
    let folder = unpacked(&work.0, "v0.1.0");
    let mut names: Vec<_> = fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["install.sh", "missive", "missive-mailto.desktop"]);

    // Checked from the folder the two files are in, as a person who downloaded them does.
    run(Command::new("sha256sum")
        .arg("--check")
        .arg("missive-v0.1.0-x86_64-linux.tar.gz.sha256")
        .current_dir(work.0.join("dist")));
}

#[test]
fn a_beta_tag_names_the_archive() {
    let work = Work::new("beta-archive");
    unpacked(&work.0, "v0.1.1-beta.21");
    assert!(
        work.0
            .join("dist/missive-v0.1.1-beta.21-x86_64-linux.tar.gz.sha256")
            .is_file()
    );
}

#[test]
fn install_in_an_empty_prefix_puts_the_binary_and_the_desktop_entry_and_leaves_the_user_alone() {
    let work = Work::new("install");
    let folder = unpacked(&work.0, "v0.1.0");
    let prefix = work.0.join("prefix");
    let output = install(&folder, &work.0, &prefix);
    let said = String::from_utf8(output.stdout).unwrap();

    let installed = prefix.join("bin/missive");
    assert!(installed.is_file());
    assert_eq!(
        fs::metadata(&installed).unwrap().permissions().mode() & 0o755,
        0o755
    );
    assert!(said.contains(&installed.display().to_string()), "{said}");
    // The installed binary tells which build it is, and install.sh shows it.
    assert!(said.contains("missive v0.1.0"), "{said}");
    // Nothing but the binary: the copy beside it was renamed, and no other folder was made.
    let in_prefix: Vec<_> = fs::read_dir(&prefix)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(in_prefix, ["bin"]);
    let in_bin: Vec<_> = fs::read_dir(prefix.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(in_bin, ["missive"]);
    // The desktop entry is in the user's data folder, not in the prefix.
    assert!(
        data(&work.0)
            .join("applications/missive-mailto.desktop")
            .is_file()
    );
    // Not a file in the home folder: no configuration, no local copy, no keyring.
    assert!(fs::read_dir(work.0.join("home")).unwrap().next().is_none());
}

#[test]
fn install_says_when_the_folder_is_not_on_the_path() {
    let work = Work::new("path");
    let folder = unpacked(&work.0, "v0.1.0");
    let prefix = work.0.join("prefix");
    let said = String::from_utf8(install(&folder, &work.0, &prefix).stdout).unwrap();
    assert!(
        said.contains(&format!(
            "Note: {} is not in your PATH.",
            prefix.join("bin").display()
        )),
        "{said}"
    );
}

#[test]
fn installing_a_newer_archive_replaces_the_binary() {
    let work = Work::new("update");
    let prefix = work.0.join("prefix");
    let older = unpacked(&work.0, "v0.1.0");
    install(&older, &work.0, &prefix);
    fs::remove_dir_all(work.0.join("target")).unwrap();
    fs::remove_dir_all(work.0.join("dist")).unwrap();
    fs::remove_dir_all(work.0.join("extracted")).unwrap();
    let newer = unpacked(&work.0, "v0.2.0");

    let said = String::from_utf8(install(&newer, &work.0, &prefix).stdout).unwrap();

    assert!(said.contains("missive v0.2.0"), "{said}");
}

#[test]
fn the_beta_version_is_the_patch_after_the_last_release_numbered_by_the_run() {
    let work = Work::new("beta-version");
    let repo = work.0.join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        run(Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .current_dir(&repo));
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "chore(repo): start"]);
    let version = |run: &str| {
        let output = run_script(&repo, run);
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };

    // No release yet: the first patch after 0.0.0.
    assert_eq!(version("7"), "0.0.1-beta.7");
    // Betas, and the other tags, never count as a release; the newest release does, sorted as
    // versions (0.10.0 is after 0.9.0).
    for tag in [
        "v0.9.0",
        "v0.10.0",
        "v0.10.1-beta.3",
        "v0.11.0-beta.1",
        "other",
    ] {
        git(&["update-ref", &format!("refs/tags/{tag}"), "HEAD"]);
    }
    assert_eq!(version("21"), "0.10.1-beta.21");
}

fn run_script(repo: &Path, number: &str) -> Output {
    run(Command::new(root().join("tools/beta-version.sh"))
        .arg(number)
        .current_dir(repo))
}

#[test]
fn the_beta_version_refuses_a_run_that_is_not_a_positive_number() {
    for bad in ["", "0", "x", "1.5"] {
        let output = Command::new(root().join("tools/beta-version.sh"))
            .arg(bad)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
    }
}

/// The keys of the `[Desktop Entry]` group of a desktop entry.
fn entry(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut in_group = false;
    let mut keys = std::collections::BTreeMap::new();
    for line in text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
        } else if in_group {
            let (key, value) = line.split_once('=').expect("a key=value line");
            assert!(
                keys.insert(key.to_owned(), value.to_owned()).is_none(),
                "{key} is given twice"
            );
        }
    }
    keys
}

#[test]
fn the_desktop_entry_makes_missive_a_handler_of_mailto_links_and_is_valid() {
    let path = root().join("dist/missive-mailto.desktop");
    let text = fs::read_to_string(&path).unwrap();
    let keys = entry(&text);
    assert_eq!(keys["Type"], "Application");
    assert!(!keys["Name"].is_empty());
    assert_eq!(keys["Exec"], "missive mailto %u");
    assert_eq!(keys["MimeType"], "x-scheme-handler/mailto;");
    // Only a handler: it is not listed in menus, and opens no terminal.
    assert_eq!(keys["NoDisplay"], "true");
    assert_eq!(keys["Terminal"], "false");
    assert!(keys["Categories"].ends_with(';'));

    // The reference validator when the machine has it (the package desktop-file-utils).
    if Command::new("desktop-file-validate")
        .arg("--version")
        .output()
        .is_ok()
    {
        run(Command::new("desktop-file-validate").arg(&path));
    }
}

#[test]
fn install_writes_the_desktop_entry_with_the_installed_program_and_tells_how_to_make_it_the_default()
 {
    let work = Work::new("desktop");
    let folder = unpacked(&work.0, "v0.1.0");
    let prefix = work.0.join("prefix");
    // A stand-in `xdg-mime` that records being called: install.sh must only say how.
    let tools = work.0.join("tools");
    fs::create_dir(&tools).unwrap();
    let called = work.0.join("xdg-mime-called");
    let stand_in = tools.join("xdg-mime");
    fs::write(
        &stand_in,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", called.display()),
    )
    .unwrap();
    fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).unwrap();
    let home = work.0.join("home");
    fs::create_dir_all(&home).unwrap();
    let output = run(Command::new("sh")
        .arg(folder.join("install.sh"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", &home)
        .env("XDG_DATA_HOME", data(&work.0))
        .env("PREFIX", &prefix));
    let said = String::from_utf8(output.stdout).unwrap();

    let installed = data(&work.0).join("applications/missive-mailto.desktop");
    let keys = entry(&fs::read_to_string(&installed).unwrap());
    // A launcher does not always have ~/.local/bin in its PATH.
    assert_eq!(
        keys["Exec"],
        format!("\"{}\" mailto %u", prefix.join("bin/missive").display())
    );
    assert_eq!(keys["MimeType"], "x-scheme-handler/mailto;");
    assert!(
        said.contains("xdg-mime default missive-mailto.desktop x-scheme-handler/mailto"),
        "{said}"
    );
    assert!(!called.exists(), "install.sh ran xdg-mime");
    // Nothing else of the data folder was made.
    let in_applications: Vec<_> = fs::read_dir(data(&work.0).join("applications"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(in_applications, ["missive-mailto.desktop"]);
}

#[test]
fn without_xdg_data_home_the_desktop_entry_goes_under_the_home_folder() {
    let work = Work::new("desktop-home");
    let folder = unpacked(&work.0, "v0.1.0");
    let home = work.0.join("home");
    fs::create_dir_all(&home).unwrap();
    run(Command::new("sh")
        .arg(folder.join("install.sh"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", &home)
        .env("PREFIX", work.0.join("prefix")));
    assert!(
        home.join(".local/share/applications/missive-mailto.desktop")
            .is_file()
    );
}
