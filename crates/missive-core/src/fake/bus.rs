//! A private session bus, for the fakes that serve a D-Bus service.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// The private `dbus-daemon` and the folder of its socket, cleaned up on drop, wherever a panic
/// happens after they exist.
pub(super) struct Daemon {
    child: Option<Child>,
    folder: PathBuf,
}

impl Daemon {
    /// Starts a bus with no service activation, so that nothing else can start on it, and gives
    /// its address.
    pub(super) fn start() -> (Daemon, String) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let folder = std::env::temp_dir().join(format!(
            "missive-fake-bus-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&folder).expect("a temporary folder");
        let mut daemon = Daemon {
            child: None,
            folder: folder.clone(),
        };
        let config = folder.join("bus.conf");
        std::fs::write(&config, bus_config(&folder)).expect("the bus configuration");
        let child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--print-address", "--nofork"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon (the dbus package) to run a private bus");
        let child = daemon.child.insert(child);
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .expect("the address of the private bus");
        let address = address.trim().to_owned();
        (daemon, address)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

/// A session bus that anyone on it may use, without the system's service files.
fn bus_config(folder: &std::path::Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_type="method_call"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
    <allow receive_type="signal"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        folder.display()
    )
}
