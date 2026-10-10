//! Where cloudmail keeps its secrets: the Secret Service (`org.freedesktop.secrets`, GNOME Keyring
//! or KeePassXC) on the session bus, and nowhere else. There is no file fallback: when the keyring
//! can't be reached, commands that need a secret fail and say so.
//!
//! Items carry the attributes `application=cloudmail` and `secret=<name>` (e.g. `api_token`), in
//! the default collection. The client follows icloud-session's (icloud-for-omarchy,
//! `sessiond/src/secrets.rs`): one bus connection and one "plain" session, since the secret only
//! crosses the local session bus, as every other call's arguments do.

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::error::{Error, ErrorKind, Result};

const APPLICATION: &str = "cloudmail";
const BUS_NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
/// The alias of the collection items are kept in (GNOME Keyring's "login").
const DEFAULT_ALIAS: &str = "default";
const DEFAULT_LABEL: &str = "Default";
const CONTENT_TYPE: &str = "text/plain";

/// The worker's API token.
pub const API_TOKEN: &str = "api_token";

/// The Secret Service's `(session, parameters, value, content_type)`.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

fn unavailable(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Config,
        format!(
            "the keyring (Secret Service) isn't available ({e}); cloudmail keeps its secrets there. Start or unlock GNOME Keyring (or another Secret Service) and try again"
        ),
    )
}

fn failure(msg: &str) -> zbus::Error {
    zbus::Error::Failure(msg.into())
}

fn attributes(name: &str) -> HashMap<&'static str, String> {
    HashMap::from([("application", APPLICATION.to_string()), ("secret", name.to_string())])
}

struct Open {
    bus: Connection,
    session: OwnedObjectPath,
}

impl Open {
    fn new() -> zbus::Result<Open> {
        let bus = Connection::session()?;
        let (_, session): (OwnedValue, OwnedObjectPath) = bus
            .call_method(Some(BUS_NAME), SERVICE_PATH, Some(SERVICE), "OpenSession", &("plain", Value::from("")))?
            .body()
            .deserialize()?;
        Ok(Open { bus, session })
    }

    fn call<R>(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Result<R>
    where
        R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        self.bus.call_method(Some(BUS_NAME), path, Some(interface), method, body)?.body().deserialize()
    }

    fn locked(&self, path: &str, interface: &str) -> zbus::Result<bool> {
        let value: OwnedValue = self.call(path, "org.freedesktop.DBus.Properties", "Get", &(interface, "Locked"))?;
        Ok(bool::try_from(value)?)
    }

    /// Runs the prompt at `path` ("/": none) and waits for its `Completed`.
    fn prompt(&self, path: &OwnedObjectPath) -> zbus::Result<Option<OwnedValue>> {
        if path.as_str() == "/" {
            return Ok(None);
        }
        let prompt = zbus::blocking::proxy::Builder::<zbus::blocking::Proxy<'_>>::new(&self.bus)
            .destination(BUS_NAME)?
            .path(path.as_str())?
            .interface(PROMPT)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()?;
        // Listening before asking: the answer can come at once.
        let mut completed = prompt.receive_signal("Completed")?;
        prompt.call_method("Prompt", &("",))?;
        let message = completed.next().ok_or_else(|| failure("the keyring prompt went away"))?;
        let (dismissed, result): (bool, OwnedValue) = message.body().deserialize()?;
        if dismissed {
            return Err(failure("the keyring prompt was dismissed"));
        }
        Ok(Some(result))
    }

    fn unlock(&self, path: &OwnedObjectPath) -> zbus::Result<()> {
        let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            self.call(SERVICE_PATH, SERVICE, "Unlock", &(vec![path],))?;
        self.prompt(&prompt)?;
        Ok(())
    }

    /// The collection with the `default` alias, made if there is none.
    fn default_collection(&self) -> zbus::Result<OwnedObjectPath> {
        let path: OwnedObjectPath = self.call(SERVICE_PATH, SERVICE, "ReadAlias", &(DEFAULT_ALIAS,))?;
        if path.as_str() != "/" {
            return Ok(path);
        }
        let properties = HashMap::from([("org.freedesktop.Secret.Collection.Label", Value::from(DEFAULT_LABEL))]);
        let (path, prompt): (OwnedObjectPath, OwnedObjectPath) =
            self.call(SERVICE_PATH, SERVICE, "CreateCollection", &(properties, DEFAULT_ALIAS))?;
        match self.prompt(&prompt)? {
            Some(made) => Ok(OwnedObjectPath::try_from(made)?),
            None => Ok(path),
        }
    }

    fn search(&self, collection: &str, attributes: &HashMap<&str, String>) -> zbus::Result<Vec<OwnedObjectPath>> {
        self.call(collection, COLLECTION, "SearchItems", &(attributes,))
    }
}

/// The secret stored under `name`, if any.
pub fn get(name: &str) -> Result<Option<String>> {
    let k = Open::new().map_err(unavailable)?;
    (|| -> zbus::Result<_> {
        for item in k.search(&k.default_collection()?, &attributes(name))? {
            if k.locked(&item, ITEM)? {
                k.unlock(&item)?;
            }
            let (_, _, value, _): Secret = k.call(&item, ITEM, "GetSecret", &(&k.session,))?;
            if let Ok(text) = String::from_utf8(value) {
                return Ok(Some(text));
            }
        }
        Ok(None)
    })()
    .map_err(unavailable)
}

/// Stores (or replaces) the secret `name`.
pub fn set(name: &str, label: &str, secret: &str) -> Result<()> {
    let k = Open::new().map_err(unavailable)?;
    (|| -> zbus::Result<_> {
        let collection = k.default_collection()?;
        if k.locked(&collection, COLLECTION)? {
            k.unlock(&collection)?;
        }
        let properties = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(label.to_string())),
            ("org.freedesktop.Secret.Item.Attributes", Value::from(attributes(name))),
        ]);
        let secret = (&k.session, Vec::<u8>::new(), secret.as_bytes(), CONTENT_TYPE);
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) =
            k.call(&collection, COLLECTION, "CreateItem", &(properties, secret, true))?;
        k.prompt(&prompt)?;
        Ok(())
    })()
    .map_err(unavailable)
}

/// Removes the secret `name`; whether there was one.
pub fn delete(name: &str) -> Result<bool> {
    let k = Open::new().map_err(unavailable)?;
    (|| -> zbus::Result<_> {
        let mut any = false;
        for item in k.search(&k.default_collection()?, &attributes(name))? {
            let prompt: OwnedObjectPath = k.call(&item, ITEM, "Delete", &())?;
            k.prompt(&prompt)?;
            any = true;
        }
        Ok(any)
    })()
    .map_err(unavailable)
}
