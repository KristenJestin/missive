//! Account passwords in the keyring, through the Secret Service API on the session bus.
//!
//! The session bus is the one of `DBUS_SESSION_BUS_ADDRESS` (else `$XDG_RUNTIME_DIR/bus`): there
//! is no other backend and no test switch, tests start their own bus with a fake Secret Service
//! on it ([`crate::fake::FakeKeyring`]). Each password is one item of the default collection,
//! with the attributes `application = missive` and `account = <id>`.
//!
//! The session is `plain`: the secret crosses the local session bus unencrypted, as with
//! libsecret's fallback. Any process able to read that bus can already ask the keyring itself.

use std::collections::HashMap;
use std::fmt;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};
use zbus::{Connection, Proxy};

use crate::account::Password;

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_INTERFACE: &str = "org.freedesktop.Secret.Prompt";
pub(crate) const LABEL: &str = "org.freedesktop.Secret.Item.Label";
pub(crate) const ATTRIBUTES: &str = "org.freedesktop.Secret.Item.Attributes";

/// A secret as the Secret Service carries it: `(oayays)`. It has no `Debug`.
#[derive(Serialize, Deserialize, Type)]
pub(crate) struct Secret {
    pub session: OwnedObjectPath,
    pub parameters: Vec<u8>,
    pub value: Vec<u8>,
    pub content_type: String,
}

#[derive(Debug)]
pub enum KeyringError {
    /// No session bus, or no Secret Service on it.
    Unavailable(String),
    /// The keyring has no default collection to keep passwords in.
    NoDefaultCollection,
    /// The user dismissed the keyring's unlock prompt.
    Dismissed,
    /// The Secret Service answered something unexpected.
    Service(String),
}

impl fmt::Display for KeyringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyringError::Unavailable(reason) => write!(
                f,
                "the keyring is unavailable: {reason} (Missive keeps passwords only in a Secret \
                 Service keyring, such as GNOME Keyring or KWallet)"
            ),
            KeyringError::NoDefaultCollection => {
                f.write_str("the keyring has no default collection to keep the password in")
            }
            KeyringError::Dismissed => f.write_str("the keyring was not unlocked"),
            KeyringError::Service(reason) => write!(f, "the keyring answered an error: {reason}"),
        }
    }
}

impl std::error::Error for KeyringError {}

impl From<zbus::Error> for KeyringError {
    fn from(error: zbus::Error) -> Self {
        match &error {
            zbus::Error::MethodError(name, _, _)
                if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
            {
                KeyringError::Unavailable("no Secret Service on the session bus".to_owned())
            }
            _ => KeyringError::Service(error.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, KeyringError>;

/// An open session with the Secret Service.
pub struct Keyring {
    connection: Connection,
    session: OwnedObjectPath,
}

impl Keyring {
    /// Opens a session on the session bus.
    pub async fn open() -> Result<Self> {
        let connection = Connection::session().await.map_err(|e| {
            KeyringError::Unavailable(format!("cannot reach the session bus ({e})"))
        })?;
        Self::open_on(connection).await
    }

    /// Opens a session on a bus already connected to.
    pub async fn open_on(connection: Connection) -> Result<Self> {
        let service = proxy(&connection, SERVICE_PATH, SERVICE_INTERFACE).await?;
        let (_, session): (OwnedValue, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::from("")))
            .await?;
        Ok(Keyring {
            connection,
            session,
        })
    }

    /// Keeps the password of an account, replacing the one it had.
    pub async fn store(&self, account: &str, password: &Password) -> Result<()> {
        let service = self.service().await?;
        let collection: OwnedObjectPath = service.call("ReadAlias", &("default",)).await?;
        if collection.as_str() == "/" {
            return Err(KeyringError::NoDefaultCollection);
        }
        self.unlock(vec![collection.clone()]).await?;
        let mut properties = HashMap::new();
        properties.insert(LABEL, Value::from(format!("Missive: {account}")));
        properties.insert(ATTRIBUTES, Value::from(attributes(account)));
        let secret = Secret {
            session: self.session.clone(),
            parameters: Vec::new(),
            value: password.expose().as_bytes().to_vec(),
            content_type: "text/plain".to_owned(),
        };
        let collection = proxy(&self.connection, collection.as_str(), COLLECTION_INTERFACE).await?;
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = collection
            .call("CreateItem", &(properties, secret, true))
            .await?;
        self.prompt(prompt).await?;
        Ok(())
    }

    /// The password of an account, if the keyring has one.
    pub async fn password(&self, account: &str) -> Result<Option<Password>> {
        let Some(item) = self.items(account).await?.into_iter().next() else {
            return Ok(None);
        };
        let item = proxy(&self.connection, item.as_str(), ITEM_INTERFACE).await?;
        let secret: Secret = item.call("GetSecret", &(&self.session,)).await?;
        let password = String::from_utf8(secret.value)
            .map_err(|_| KeyringError::Service("the password is not UTF-8".to_owned()))?;
        Ok(Some(Password::new(password)))
    }

    /// Deletes every keyring entry of an account. `false` when there was none.
    pub async fn delete(&self, account: &str) -> Result<bool> {
        let items = self.items(account).await?;
        for item in &items {
            let item = proxy(&self.connection, item.as_str(), ITEM_INTERFACE).await?;
            let prompt: OwnedObjectPath = item.call("Delete", &()).await?;
            self.prompt(prompt).await?;
        }
        Ok(!items.is_empty())
    }

    /// The items of an account, unlocked.
    async fn items(&self, account: &str) -> Result<Vec<OwnedObjectPath>> {
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self
            .service()
            .await?
            .call("SearchItems", &(attributes(account),))
            .await?;
        if !locked.is_empty() {
            unlocked.extend(self.unlock(locked).await?);
        }
        Ok(unlocked)
    }

    async fn unlock(&self, objects: Vec<OwnedObjectPath>) -> Result<Vec<OwnedObjectPath>> {
        let (mut unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            self.service().await?.call("Unlock", &(objects,)).await?;
        if let Some(result) = self.prompt(prompt).await? {
            let more = Vec::<OwnedObjectPath>::try_from(result)
                .map_err(|e| KeyringError::Service(e.to_string()))?;
            unlocked.extend(more);
        }
        Ok(unlocked)
    }

    /// Shows a prompt of the keyring (to unlock it, most often) and waits for the user. `None`
    /// when there was nothing to prompt, else what the prompt completed with.
    async fn prompt(&self, prompt: OwnedObjectPath) -> Result<Option<OwnedValue>> {
        if prompt.as_str() == "/" {
            return Ok(None);
        }
        let prompt = proxy(&self.connection, prompt.as_str(), PROMPT_INTERFACE).await?;
        let mut completed = prompt.receive_signal("Completed").await?;
        prompt.call_method("Prompt", &("",)).await?;
        let message = completed
            .next()
            .await
            .ok_or_else(|| KeyringError::Service("the prompt vanished".to_owned()))?;
        let (dismissed, result): (bool, OwnedValue) = message.body().deserialize()?;
        if dismissed {
            return Err(KeyringError::Dismissed);
        }
        Ok(Some(result))
    }

    async fn service(&self) -> Result<Proxy<'static>> {
        proxy(&self.connection, SERVICE_PATH, SERVICE_INTERFACE).await
    }
}

fn attributes(account: &str) -> HashMap<&str, &str> {
    HashMap::from([("application", "missive"), ("account", account)])
}

async fn proxy(connection: &Connection, path: &str, interface: &str) -> Result<Proxy<'static>> {
    let path =
        ObjectPath::try_from(path.to_owned()).map_err(|e| KeyringError::Service(e.to_string()))?;
    Ok(zbus::proxy::Builder::new(connection)
        .destination(SERVICE)?
        .path(path)?
        .interface(interface.to_owned())?
        // Nothing here reads properties; fetching them would be one more call per object.
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}
