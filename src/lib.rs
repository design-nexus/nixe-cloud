//! Public Cloud1 metadata and asynchronous session-bus client. No credential fields.
use serde::{Deserialize, Serialize};

pub const BUS: &str = "io.github.design_nexus.nixe.Cloud1";
pub const PATH: &str = "/io/github/design_nexus/nixe/Cloud1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preferences {
    pub mount_on_login: bool,
    pub cache_mib: u64,
    /// rclone bandwidth specification; empty means unlimited.
    pub bandwidth: String,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            mount_on_login: false,
            cache_mib: 2048,
            bandwidth: String::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub remote_name: String,
    pub adopted: bool,
    pub managed: bool,
    pub connection: String,
    pub mount_status: String,
    pub mount_path: String,
    pub error: String,
    pub preferences: Preferences,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub name: String,
    pub provider: String,
}

#[zbus::proxy(
    interface = "io.github.design_nexus.nixe.Cloud1",
    default_service = "io.github.design_nexus.nixe.Cloud1",
    default_path = "/io/github/design_nexus/nixe/Cloud1"
)]
pub trait Cloud {
    fn cancel(&self, operation: &str) -> zbus::Result<()>;
    fn list_remotes(&self) -> zbus::Result<String>;
    fn discover(&self) -> zbus::Result<String>;
    fn adopt(&self, name: &str) -> zbus::Result<String>;
    fn mount(&self, id: &str) -> zbus::Result<String>;
    fn unmount(&self, id: &str) -> zbus::Result<String>;
    fn test_connection(&self, id: &str) -> zbus::Result<String>;
    fn update_preferences(&self, id: &str, name: &str, preferences: &str) -> zbus::Result<()>;
    fn remove(&self, id: &str, delete_configuration: bool) -> zbus::Result<()>;
    fn begin_setup(&self) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn operation_finished(
        &self,
        operation: &str,
        id: &str,
        success: bool,
        message: &str,
    ) -> zbus::Result<()>;
}

/// Blocking adapter intended for application worker threads, never GTK's main thread.
pub struct Client {
    runtime: tokio::runtime::Runtime,
}
impl Client {
    pub fn connect() -> anyhow::Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }
    pub fn list(&self) -> anyhow::Result<Vec<Remote>> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            let p = CloudProxy::new(&c).await?;
            Ok(serde_json::from_str(&p.list_remotes().await?)?)
        })
    }
    pub fn discover(&self) -> anyhow::Result<Vec<Discovery>> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            let p = CloudProxy::new(&c).await?;
            Ok(serde_json::from_str(&p.discover().await?)?)
        })
    }
    pub fn adopt(&self, name: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c).await?.adopt(name).await?)
        })
    }
    pub fn mount(&self, id: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c).await?.mount(id).await?)
        })
    }
    pub fn unmount(&self, id: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c).await?.unmount(id).await?)
        })
    }
    pub fn test(&self, id: &str) -> anyhow::Result<String> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c).await?.test_connection(id).await?)
        })
    }
    pub fn update(&self, id: &str, name: &str, prefs: &Preferences) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c)
                .await?
                .update_preferences(id, name, &serde_json::to_string(prefs)?)
                .await?)
        })
    }
    pub fn remove(&self, id: &str, delete: bool) -> anyhow::Result<()> {
        self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok(CloudProxy::new(&c).await?.remove(id, delete).await?)
        })
    }
    pub fn setup(&self) -> anyhow::Result<SetupSession> {
        let path = self.runtime.block_on(async {
            let c = zbus::Connection::session().await?;
            Ok::<_, anyhow::Error>(CloudProxy::new(&c).await?.begin_setup().await?)
        })?;
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(900)))?;
        Ok(SetupSession(std::io::BufReader::new(stream)))
    }
}
pub struct SetupSession(std::io::BufReader<std::os::unix::net::UnixStream>);
impl SetupSession {
    /// Requests and replies are deliberately never logged or included in error messages.
    pub fn request(&mut self, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        use std::io::{BufRead, Write};
        serde_json::to_writer(self.0.get_mut(), &request)?;
        self.0.get_mut().write_all(b"\n")?;
        let mut line = String::new();
        self.0.read_line(&mut line)?;
        let response: serde_json::Value = serde_json::from_str(&line)
            .map_err(|_| anyhow::anyhow!("Cloud setup stopped. Retry adding the account."))?;
        if response.get("error").is_some() {
            anyhow::bail!("Setup failed. Check account details and authorization.");
        }
        Ok(response)
    }
}
