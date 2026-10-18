//! A fake Secret Service on a private session bus: see [`FakeKeyring`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, fdo, interface};

use crate::keyring::{ATTRIBUTES, LABEL, Secret};

const COLLECTION: &str = "/org/freedesktop/secrets/collection/login";

/// A private `dbus-daemon` with a fake `org.freedesktop.secrets` on it, for tests that need a
/// keyring and must never touch the real one.
///
/// Run the program under test with `DBUS_SESSION_BUS_ADDRESS` set to [`FakeKeyring::address`].
/// The bus has no service activation, so nothing else can start on it. The fake serves what
/// Missive's client uses, with a `plain` session and one default collection: `OpenSession`,
/// `ReadAlias`, `SearchItems` and `Unlock` on the service, `CreateItem` on the collection,
/// `GetSecret` and `Delete` on items, `Prompt` on prompts.
///
/// [`FakeKeyring::start`] gives an unlocked collection, which never prompts. With
/// [`FakeKeyring::start_locked`] the collection starts locked: its items are only found as
/// locked, `Unlock` answers with a prompt object, and the prompt completes the way the test
/// chose ([`PromptAnswer`]). A prompt is a D-Bus object on the private bus: nothing is ever shown
/// on a screen. The bus and the fake stop when it is dropped; it needs a running tokio runtime.
pub struct FakeKeyring {
    // Dropped first: the connection goes before the bus it is on.
    _connection: Connection,
    _daemon: Daemon,
    address: String,
    items: Arc<Mutex<Vec<StoredItem>>>,
    lock: Arc<Lock>,
}

/// What the user does with the unlock prompt of a locked [`FakeKeyring`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    /// The prompt completes with `dismissed = false` and the collection is unlocked.
    Accept,
    /// The prompt completes with `dismissed = true` and the collection stays locked.
    Dismiss,
}

/// The private `dbus-daemon` and the folder of its socket, cleaned up on drop, wherever a panic
/// happens after they exist.
struct Daemon {
    child: Option<Child>,
    folder: PathBuf,
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

/// Whether the collection is locked, and what the user answers to the prompt that unlocks it.
struct Lock {
    locked: AtomicBool,
    answer: PromptAnswer,
    /// How many times a secret was read (`GetSecret`).
    secret_reads: AtomicUsize,
}

impl Lock {
    fn refuse_if_locked(&self) -> fdo::Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed(
                "org.freedesktop.Secret.Error.IsLocked".to_owned(),
            ));
        }
        Ok(())
    }
}

/// An entry of the fake keyring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeItem {
    pub label: String,
    pub attributes: HashMap<String, String>,
    pub secret: Vec<u8>,
}

struct StoredItem {
    path: OwnedObjectPath,
    item: FakeItem,
}

impl FakeKeyring {
    /// A keyring whose default collection is unlocked.
    pub async fn start() -> Self {
        Self::start_with(Lock {
            locked: AtomicBool::new(false),
            answer: PromptAnswer::Accept,
            secret_reads: AtomicUsize::new(0),
        })
        .await
    }

    /// A keyring whose default collection is locked until its prompt is accepted.
    pub async fn start_locked(answer: PromptAnswer) -> Self {
        Self::start_with(Lock {
            locked: AtomicBool::new(true),
            answer,
            secret_reads: AtomicUsize::new(0),
        })
        .await
    }

    async fn start_with(lock: Lock) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let folder = std::env::temp_dir().join(format!(
            "missive-fake-keyring-{}-{}",
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

        let items = Arc::new(Mutex::new(Vec::new()));
        let lock = Arc::new(lock);
        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("a bus address")
            .name("org.freedesktop.secrets")
            .expect("a bus name")
            .serve_at(
                "/org/freedesktop/secrets",
                Service {
                    items: items.clone(),
                    lock: lock.clone(),
                },
            )
            .expect("the service object")
            .serve_at(
                COLLECTION,
                Collection {
                    items: items.clone(),
                    lock: lock.clone(),
                },
            )
            .expect("the collection object")
            .build()
            .await
            .expect("a connection to the private bus");

        FakeKeyring {
            _connection: connection,
            _daemon: daemon,
            address,
            items,
            lock,
        }
    }

    /// The address of the private bus, for `DBUS_SESSION_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// How many times a client read a secret (`GetSecret`), whichever entry it was.
    pub fn secret_reads(&self) -> usize {
        self.lock.secret_reads.load(Ordering::SeqCst)
    }

    /// Every entry, in the order they were created.
    pub fn items(&self) -> Vec<FakeItem> {
        lock(&self.items).iter().map(|s| s.item.clone()).collect()
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

fn lock(items: &Mutex<Vec<StoredItem>>) -> MutexGuard<'_, Vec<StoredItem>> {
    items
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn path(path: String) -> OwnedObjectPath {
    ObjectPath::try_from(path)
        .expect("a valid object path")
        .into()
}

fn invalid(error: zbus::zvariant::Error) -> fdo::Error {
    fdo::Error::InvalidArgs(error.to_string())
}

fn no_prompt() -> OwnedObjectPath {
    path("/".to_owned())
}

struct Service {
    items: Arc<Mutex<Vec<StoredItem>>>,
    lock: Arc<Lock>,
}

#[interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    fn open_session(
        &self,
        algorithm: &str,
        _input: OwnedValue,
    ) -> fdo::Result<(OwnedValue, OwnedObjectPath)> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        if algorithm != "plain" {
            return Err(fdo::Error::NotSupported(format!(
                "the fake keyring only has plain sessions, not {algorithm}"
            )));
        }
        let session = path(format!(
            "/org/freedesktop/secrets/session/{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let output = Value::from("")
            .try_to_owned()
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok((output, session))
    }

    fn read_alias(&self, name: &str) -> OwnedObjectPath {
        if name == "default" {
            path(COLLECTION.to_owned())
        } else {
            no_prompt()
        }
    }

    fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
        let found = lock(&self.items)
            .iter()
            .filter(|s| {
                attributes
                    .iter()
                    .all(|(k, v)| s.item.attributes.get(k) == Some(v))
            })
            .map(|s| s.path.clone())
            .collect();
        if self.lock.locked.load(Ordering::SeqCst) {
            (Vec::new(), found)
        } else {
            (found, Vec::new())
        }
    }

    async fn unlock(
        &self,
        objects: Vec<OwnedObjectPath>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        if !self.lock.locked.load(Ordering::SeqCst) {
            return Ok((objects, no_prompt()));
        }
        let prompt = path(format!(
            "/org/freedesktop/secrets/prompt/u{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        server
            .at(
                &prompt,
                Prompt {
                    lock: self.lock.clone(),
                    objects,
                },
            )
            .await?;
        Ok((Vec::new(), prompt))
    }
}

/// The unlock prompt: it completes as the test chose, from a task of its own as a real one
/// answers after `Prompt` returned.
struct Prompt {
    lock: Arc<Lock>,
    objects: Vec<OwnedObjectPath>,
}

#[interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    fn prompt(&self, _window_id: &str, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        let emitter = emitter.into_owned();
        let lock = self.lock.clone();
        let objects = self.objects.clone();
        tokio::spawn(async move {
            let (dismissed, unlocked) = match lock.answer {
                PromptAnswer::Accept => {
                    lock.locked.store(false, Ordering::SeqCst);
                    (false, objects)
                }
                PromptAnswer::Dismiss => (true, Vec::new()),
            };
            let result = Value::from(
                unlocked
                    .iter()
                    .map(|object| object.as_ref())
                    .collect::<Vec<ObjectPath<'_>>>(),
            );
            let _ = Self::completed(&emitter, dismissed, result).await;
        });
    }

    fn dismiss(&self) {}

    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Value<'_>,
    ) -> zbus::Result<()>;
}

struct Collection {
    items: Arc<Mutex<Vec<StoredItem>>>,
    lock: Arc<Lock>,
}

#[interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    async fn create_item(
        &self,
        properties: HashMap<String, OwnedValue>,
        secret: Secret,
        replace: bool,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        self.lock.refuse_if_locked()?;
        let label = match properties.get(LABEL) {
            Some(label) => label
                .try_clone()
                .and_then(String::try_from)
                .map_err(invalid)?,
            None => String::new(),
        };
        let attributes = match properties.get(ATTRIBUTES) {
            Some(attributes) => attributes
                .try_clone()
                .and_then(HashMap::<String, String>::try_from)
                .map_err(invalid)?,
            None => HashMap::new(),
        };
        let item = FakeItem {
            label,
            attributes,
            secret: secret.value,
        };
        if replace {
            let mut items = lock(&self.items);
            if let Some(stored) = items
                .iter_mut()
                .find(|s| s.item.attributes == item.attributes)
            {
                stored.item = item;
                return Ok((stored.path.clone(), no_prompt()));
            }
        }
        let item_path = path(format!(
            "{COLLECTION}/{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        server
            .at(
                &item_path,
                Item {
                    items: self.items.clone(),
                    lock: self.lock.clone(),
                },
            )
            .await?;
        lock(&self.items).push(StoredItem {
            path: item_path.clone(),
            item,
        });
        Ok((item_path, no_prompt()))
    }
}

struct Item {
    items: Arc<Mutex<Vec<StoredItem>>>,
    lock: Arc<Lock>,
}

#[interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    fn get_secret(
        &self,
        session: OwnedObjectPath,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<Secret> {
        self.lock.refuse_if_locked()?;
        self.lock.secret_reads.fetch_add(1, Ordering::SeqCst);
        let me = header.path().map(|p| p.as_str().to_owned());
        let items = lock(&self.items);
        let stored = items
            .iter()
            .find(|s| Some(s.path.as_str()) == me.as_deref())
            .ok_or_else(|| fdo::Error::UnknownObject("no such item".to_owned()))?;
        Ok(Secret {
            session,
            parameters: Vec::new(),
            value: stored.item.secret.clone(),
            content_type: "text/plain".to_owned(),
        })
    }

    async fn delete(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let Some(me) = header.path().map(|p| p.to_owned()) else {
            return Err(fdo::Error::UnknownObject("no such item".to_owned()));
        };
        lock(&self.items).retain(|s| s.path.as_str() != me.as_str());
        server.remove::<Item, _>(me).await?;
        Ok(no_prompt())
    }
}
