use anyhow::{Context, Result, bail};
use nixe_cloud::{BUS, Discovery, PATH, Preferences, Remote};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use zbus::{interface, object_server::SignalEmitter};

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set")
}
fn xdg(key: &str, fallback: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}
fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("nixe/cloud")
}
fn config() -> PathBuf {
    config_dir().join("rclone.conf")
}
fn mounts() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("nixe/cloud/mounts")
}
fn cache(id: &str) -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("nixe/cloud").join(id)
}
fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(home())?.uid();
    if !meta.is_dir() || meta.uid() != uid {
        bail!("Unsafe Cloud directory");
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn id_valid(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}
fn external_config() -> Option<PathBuf> {
    let output = std::process::Command::new("rclone")
        .args(["config", "file"])
        .output()
        .ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .last()
        .map(PathBuf::from)
}
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    remotes: Vec<Remote>,
}
struct State {
    version: String,
    setup_gate: Mutex<()>,
    remotes: Mutex<Vec<Remote>>,
    children: Mutex<HashMap<String, Child>>,
    busy: Mutex<HashSet<String>>,
    rc: reqwest::Client,
    connection: zbus::Connection,
    tasks: Mutex<HashMap<String, (String, tokio::task::AbortHandle)>>,
}
impl State {
    async fn notify(&self) {
        if let Ok(e) = SignalEmitter::new(&self.connection, PATH) {
            let _ = Cloud::changed(&e).await;
        }
    }
    async fn save(&self) -> Result<()> {
        let remotes = self.remotes.lock().await;
        let bytes = serde_json::to_vec_pretty(&Saved {
            remotes: remotes.clone(),
        })?;
        let tmp = config_dir().join("accounts.json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        tokio::fs::rename(tmp, config_dir().join("accounts.json")).await?;
        Ok(())
    }
    async fn get(&self, id: &str) -> Result<Remote> {
        if !id_valid(id) {
            bail!("Invalid account identifier");
        }
        self.remotes
            .lock()
            .await
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .context("Account no longer exists")
    }
    async fn status(&self, id: &str, connection: &str, mount: &str, error: &str) {
        if let Some(r) = self.remotes.lock().await.iter_mut().find(|r| r.id == id) {
            r.connection = connection.into();
            r.mount_status = mount.into();
            r.error = error.into();
        }
        self.notify().await;
    }
    fn command(&self, r: &Remote) -> Result<Command> {
        let path = if r.managed {
            config()
        } else {
            external_config().context("rclone configuration unavailable")?
        };
        let mut cmd = Command::new("rclone");
        cmd.arg("--config")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        Ok(cmd)
    }
    async fn rc(&self, endpoint: &str, body: Value) -> Result<Value> {
        let response = self
            .rc
            .post(format!("http://localhost/{endpoint}"))
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "rclone could not complete configuration; check account details and authorization"
            );
        }
        Ok(response.json().await?)
    }
    async fn discover(&self) -> Result<Vec<Discovery>> {
        let output = Command::new("rclone")
            .args(["listremotes", "--long"])
            .output()
            .await?;
        if !output.status.success() {
            bail!("Unable to read rclone configuration; it may need unlocking");
        }
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .filter_map(|l| {
                let (name, provider) = l.split_once(':')?;
                Some(Discovery {
                    name: name.into(),
                    provider: provider.trim().into(),
                })
            })
            .collect())
    }
    async fn start_mount(&self, id: &str) -> Result<()> {
        let r = self.get(id).await?;
        self.test(id).await?;
        if mounted(&r.mount_path).await && !self.children.lock().await.contains_key(id) {
            let disconnected = std::fs::metadata(&r.mount_path)
                .err()
                .is_some_and(|e| e.raw_os_error() == Some(107));
            if disconnected {
                let status = Command::new("fusermount3")
                    .args(["-u", &r.mount_path])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await?;
                if !status.success() {
                    bail!("Previous mount is busy. Close open files and retry.");
                }
            }
        }
        if self.children.lock().await.contains_key(id) && mounted(&r.mount_path).await {
            return Ok(());
        }
        let path = PathBuf::from(&r.mount_path);
        if path != mounts().join(id) {
            bail!("Unsafe mount path");
        }
        private_dir(&mounts())?;
        if mounted(&r.mount_path).await {
            bail!("Mountpoint already in use");
        }
        if let Ok(m) = std::fs::symlink_metadata(&path) {
            if !m.is_dir()
                || m.file_type().is_symlink()
                || std::fs::read_dir(&path)?.next().is_some()
            {
                bail!("Mountpoint is not an empty directory");
            }
        }
        private_dir(&path)?;
        private_dir(&cache(id))?;
        self.status(id, "connecting", "mounting", "").await;
        let mut command = self.command(&r)?;
        command
            .args([
                "mount",
                &format!("{}:", r.remote_name),
                &r.mount_path,
                "--vfs-cache-mode",
                "full",
                "--vfs-cache-max-size",
                &format!("{}M", r.preferences.cache_mib),
                "--cache-dir",
            ])
            .arg(cache(id))
            .args(["--umask", "077", "--log-level", "ERROR"]);
        if !r.preferences.bandwidth.is_empty() {
            command.args(["--bwlimit", &r.preferences.bandwidth]);
        }
        let socket = runtime()?.join(format!("mount-{id}.sock"));
        if socket.exists() {
            tokio::fs::remove_file(&socket).await?;
        }
        command
            .args(["--rc", "--rc-no-auth", "--rc-addr"])
            .arg(format!("unix://{}", socket.display()));
        command
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        for _ in 0..150 {
            if mounted(&r.mount_path).await {
                self.children.lock().await.insert(id.into(), child);
                self.status(id, "connected", "mounted", "").await;
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                bail!("Mount failed. Test the connection or sign in again.");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = child.kill().await;
        bail!("Mount timed out. Check the network and retry.")
    }
    async fn stop_mount(&self, id: &str) -> Result<()> {
        let r = self.get(id).await?;
        // A normal FUSE unmount refuses busy files; never force or lazily detach.
        if mounted(&r.mount_path).await {
            // Pending dirty files cannot safely be inferred from a successful copy.
            let client = reqwest::Client::builder()
                .unix_socket(runtime()?.join(format!("mount-{id}.sock")))
                .timeout(Duration::from_secs(3))
                .build()?;
            let stats: Value = client
                .post("http://localhost/vfs/stats")
                .json(&json!({}))
                .send()
                .await
                .context("Unable to verify upload status; mount left connected")?
                .error_for_status()?
                .json()
                .await?;
            let disk = &stats["diskCache"];
            if ["uploadsInProgress", "uploadsQueued", "erroredFiles"]
                .iter()
                .any(|key| disk[*key].as_u64().unwrap_or(1) > 0)
            {
                bail!(
                    "Files are open or uploads are pending. Close files and wait for uploads before disconnecting."
                );
            }
            let status = Command::new("fusermount3")
                .args(["-u", &r.mount_path])
                .status()
                .await?;
            if !status.success() {
                bail!("Mount is busy. Close open files and retry.");
            }
        }
        if let Some(mut child) = self.children.lock().await.remove(id) {
            if tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
        self.status(id, "disconnected", "unmounted", "").await;
        Ok(())
    }
    async fn test(&self, id: &str) -> Result<()> {
        let r = self.get(id).await?;
        let mut cmd = self.command(&r)?;
        cmd.kill_on_drop(true);
        cmd.args([
            "lsjson",
            &format!("{}:", r.remote_name),
            "--max-depth",
            "1",
            "--dirs-only",
            "--contimeout",
            "10s",
            "--timeout",
            "20s",
            "--retries",
            "1",
        ]);
        let output = tokio::time::timeout(Duration::from_secs(35), cmd.output()).await??;
        if !output.status.success() {
            let msg = String::from_utf8_lossy(&output.stderr).to_lowercase();
            let auth = [
                "invalid_grant",
                "unauthorized",
                "401",
                "authentication failed",
                "token expired",
            ]
            .iter()
            .any(|x| msg.contains(x));
            self.status(
                id,
                if auth {
                    "authentication_required"
                } else {
                    "error"
                },
                &r.mount_status,
                if auth {
                    "Sign in again to reconnect."
                } else {
                    "Connection failed. Check the network and account settings."
                },
            )
            .await;
            bail!("Connection test failed; see account status");
        }
        self.status(id, "connected", &r.mount_status, "").await;
        Ok(())
    }
}
async fn mounted(path: &str) -> bool {
    tokio::fs::read_to_string("/proc/self/mountinfo")
        .await
        .unwrap_or_default()
        .lines()
        .any(|line| line.split_whitespace().nth(4) == Some(path))
}
struct Cloud(Arc<State>);
fn error(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}
impl Cloud {
    async fn launch(&self, id: &str, kind: &'static str) -> zbus::fdo::Result<String> {
        self.0.get(id).await.map_err(error)?;
        if !self.0.busy.lock().await.insert(id.into()) {
            return Err(error("Account operation already in progress"));
        }
        let op = uuid::Uuid::new_v4().to_string();
        let task_op = op.clone();
        let account_id = id.to_owned();
        let id = id.to_owned();
        let state = self.0.clone();
        let handle = tokio::spawn(async move {
            let result = match kind {
                "mount" => state.start_mount(&id).await,
                "unmount" => state.stop_mount(&id).await,
                _ => state.test(&id).await,
            };
            let message = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            if result.is_err() && kind == "mount" {
                let auth = state
                    .get(&id)
                    .await
                    .is_ok_and(|r| r.connection == "authentication_required");
                if !auth {
                    state.status(&id, "error", "unmounted", &message).await;
                }
            }
            state.busy.lock().await.remove(&id);
            state.tasks.lock().await.remove(&task_op);
            if let Ok(e) = SignalEmitter::new(&state.connection, PATH) {
                let _ =
                    Cloud::operation_finished(&e, &task_op, &id, result.is_ok(), &message).await;
            }
            state.notify().await;
        });
        self.0
            .tasks
            .lock()
            .await
            .insert(op.clone(), (account_id, handle.abort_handle()));
        Ok(op)
    }
}
#[interface(name = "io.github.design_nexus.nixe.Cloud1")]
impl Cloud {
    #[zbus(property)]
    fn rclone_version(&self) -> &str {
        &self.0.version
    }
    async fn cancel(&self, operation: &str) -> zbus::fdo::Result<()> {
        if let Some((id, handle)) = self.0.tasks.lock().await.remove(operation) {
            handle.abort();
            self.0.busy.lock().await.remove(&id);
            let is_mounted = self.0.get(&id).await.ok().is_some_and(|r| {
                std::fs::read_to_string("/proc/self/mountinfo")
                    .unwrap_or_default()
                    .lines()
                    .any(|line| line.split_whitespace().nth(4) == Some(r.mount_path.as_str()))
            });
            self.0
                .status(
                    &id,
                    if is_mounted {
                        "connected"
                    } else {
                        "disconnected"
                    },
                    if is_mounted { "mounted" } else { "unmounted" },
                    "Operation cancelled. Any cached data is preserved.",
                )
                .await;
        }
        Ok(())
    }
    async fn list_remotes(&self) -> zbus::fdo::Result<String> {
        serde_json::to_string(&*self.0.remotes.lock().await).map_err(error)
    }
    async fn discover(&self) -> zbus::fdo::Result<String> {
        serde_json::to_string(&self.0.discover().await.map_err(error)?).map_err(error)
    }
    async fn adopt(&self, name: &str) -> zbus::fdo::Result<String> {
        let d = self
            .0
            .discover()
            .await
            .map_err(error)?
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| error("Remote not found"))?;
        let mut remotes = self.0.remotes.lock().await;
        if let Some(r) = remotes.iter().find(|r| !r.managed && r.remote_name == name) {
            return Ok(r.id.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        remotes.push(new_remote(&id, name, name, &d.provider, false));
        drop(remotes);
        self.0.save().await.map_err(error)?;
        self.0.notify().await;
        Ok(id)
    }
    async fn mount(&self, id: &str) -> zbus::fdo::Result<String> {
        self.launch(id, "mount").await
    }
    async fn unmount(&self, id: &str) -> zbus::fdo::Result<String> {
        self.launch(id, "unmount").await
    }
    async fn test_connection(&self, id: &str) -> zbus::fdo::Result<String> {
        self.launch(id, "test").await
    }
    async fn update_preferences(
        &self,
        id: &str,
        name: &str,
        preferences: &str,
    ) -> zbus::fdo::Result<()> {
        let prefs: Preferences =
            serde_json::from_str(preferences).map_err(|_| error("Invalid preferences"))?;
        if name.trim().is_empty()
            || prefs.cache_mib == 0
            || prefs.bandwidth.len() > 128
            || prefs.bandwidth.contains(['\n', '\r'])
        {
            return Err(error("Invalid account preferences"));
        }
        let mut remotes = self.0.remotes.lock().await;
        let r = remotes
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| error("Account not found"))?;
        r.name = name.trim().into();
        r.preferences = prefs;
        drop(remotes);
        self.0.save().await.map_err(error)?;
        self.0.notify().await;
        Ok(())
    }
    async fn remove(&self, id: &str, delete_configuration: bool) -> zbus::fdo::Result<()> {
        let r = self.0.get(id).await.map_err(error)?;
        if self.0.busy.lock().await.contains(id) {
            return Err(error("Wait for the current account operation"));
        }
        self.0.stop_mount(id).await.map_err(error)?;
        if delete_configuration {
            if !r.managed {
                return Err(error(
                    "External configuration is preserved. Remove it with its original configuration tool.",
                ));
            }
            self.0
                .rc("config/delete", json!({"name": r.remote_name}))
                .await
                .map_err(error)?;
        }
        self.0.remotes.lock().await.retain(|r| r.id != id);
        self.0.save().await.map_err(error)?;
        self.0.notify().await;
        Ok(())
    }
    async fn begin_setup(&self) -> zbus::fdo::Result<String> {
        let dir = runtime().map_err(error)?;
        let path = dir.join(format!("setup-{}.sock", uuid::Uuid::new_v4()));
        let listener = tokio::net::UnixListener::bind(&path).map_err(error)?;
        let state = self.0.clone();
        let task_path = path.clone();
        tokio::spawn(async move {
            let _ = setup(state, listener).await;
            let _ = tokio::fs::remove_file(task_path).await;
        });
        Ok(path.to_string_lossy().into())
    }
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn operation_finished(
        emitter: &SignalEmitter<'_>,
        operation: &str,
        id: &str,
        success: bool,
        message: &str,
    ) -> zbus::Result<()>;
}
fn new_remote(id: &str, display: &str, remote: &str, provider: &str, managed: bool) -> Remote {
    Remote {
        id: id.into(),
        name: display.into(),
        provider: provider.into(),
        remote_name: remote.into(),
        adopted: !managed,
        managed,
        connection: "disconnected".into(),
        mount_status: "unmounted".into(),
        mount_path: mounts().join(id).to_string_lossy().into(),
        error: String::new(),
        preferences: Preferences::default(),
    }
}
fn runtime() -> Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").context("User runtime directory unavailable")?,
    )
    .join("nixe-cloud");
    private_dir(&path)?;
    Ok(path)
}
// Setup is a single private session. Only configuration operations are allowed.
async fn setup(state: Arc<State>, listener: tokio::net::UnixListener) -> Result<()> {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(900), listener.accept()).await??;
    let _setup_guard = state.setup_gate.lock().await;
    let peer = stream.peer_cred()?;
    use std::os::unix::fs::MetadataExt;
    if peer.uid() != std::fs::metadata(home())?.uid() {
        bail!("Wrong setup user");
    }
    let mut stream = BufReader::new(stream);
    let remote = format!("nixe_{}", uuid::Uuid::new_v4().simple());
    let mut created = false;
    let mut replacement: Option<Remote> = None;
    let result: Result<()> = async {
        loop {
            let mut line = String::new();
            if tokio::time::timeout(Duration::from_secs(900), stream.read_line(&mut line)).await?? == 0 { break; }
            if line.len() > 1024 * 1024 { bail!("Setup request too large"); }
            let request: Value = serde_json::from_str(&line)?;
            let response = match request["action"].as_str().unwrap_or("") {
                "providers" => state.rc("config/providers", json!({})).await?,
                "configure" => {
                    let provider = request["provider"].as_str().context("Provider required")?;
                    let mut parameters = request["parameters"].clone();
                    if !created && let Some(id) = request["replace"].as_str() {
                        let old = state.get(id).await?;
                        if !old.managed { bail!("External credentials are preserved"); }
                        if mounted(&old.mount_path).await { bail!("Disconnect the account before signing in again"); }
                        if !state.busy.lock().await.insert(id.into()) { bail!("Account is busy"); }
                        replacement = Some(old.clone());
                        let previous = state.rc("config/get", json!({"name":old.remote_name})).await?;
                        let mut merged = previous;
                        if let Some(values) = parameters.as_object() { for (key, value) in values { merged[key] = value.clone(); } }
                        if let Some(values) = merged.as_object_mut() { values.remove("type"); }
                        merged["config_refresh_token"] = json!("true");
                        parameters = merged;
                    }
                    let mut body = json!({"name":remote,"type":provider,"parameters":parameters,"opt":{"nonInteractive":true,"obscure":replacement.is_none()}});
                    if let Some(opt) = request.get("opt") { body["opt"] = opt.clone(); body["opt"]["nonInteractive"] = json!(true); }
                    let endpoint = if created { "config/update" } else { "config/create" };
                    created = true;
                    tokio::select! {
                        result = state.rc(endpoint, body) => result?,
                        peer = stream.fill_buf() => {
                            let _ = peer?;
                            let _ = state.rc("config/oauthstop", json!({})).await;
                            bail!("Setup cancelled");
                        }
                    }
                },
                "oauth" => state.rc("config/oauthstatus", json!({})).await?,
                "finish" => {
                    if !created { bail!("Configure the account first"); }
                    let id = uuid::Uuid::new_v4().to_string();
                    let name = request["name"].as_str().filter(|s| !s.trim().is_empty()).context("Display name required")?;
                    let provider = request["provider"].as_str().context("Provider required")?;
                    let mut r = new_remote(&id, name, &remote, provider, true);
                    if let Some(prefs) = request.get("preferences") { r.preferences = serde_json::from_value(prefs.clone())?; }
                    if r.preferences.cache_mib == 0 { bail!("Invalid cache size"); }
                    state.remotes.lock().await.push(r);
                    let tested = tokio::select! {
                        result = state.test(&id) => result,
                        _ = stream.fill_buf() => Err(anyhow::anyhow!("Setup cancelled")),
                    };
                    if let Err(e) = tested { state.remotes.lock().await.retain(|r| r.id != id); return Err(e); }
                    let final_id = if let Some(old) = replacement.as_ref() {
                        let mut remotes = state.remotes.lock().await;
                        remotes.retain(|r| r.id != id);
                        let current = remotes.iter_mut().find(|r| r.id == old.id).context("Account removed during setup")?;
                        current.remote_name = remote.clone(); current.name = name.into(); current.connection = "connected".into(); current.error.clear();
                        old.id.clone()
                    } else { id };
                    if let Err(e) = state.save().await {
                        let mut remotes = state.remotes.lock().await;
                        if let Some(old) = replacement.as_ref() {
                            if let Some(current) = remotes.iter_mut().find(|r| r.id == old.id) { *current = old.clone(); }
                        } else { remotes.retain(|r| r.id != final_id); }
                        return Err(e);
                    }
                    created = false;
                    state.notify().await;
                    if let Some(old) = replacement.as_ref() { let _ = state.rc("config/delete", json!({"name":old.remote_name})).await; }
                    stream.get_mut().write_all(format!("{}\n", json!({"id":final_id})).as_bytes()).await?;
                    break;
                },
                _ => bail!("Unknown setup action"),
            };
            stream.get_mut().write_all(format!("{response}\n").as_bytes()).await?;
        }
        Ok(())
    }.await;
    if let Some(old) = replacement {
        state.busy.lock().await.remove(&old.id);
    }
    if created {
        let _ = state.rc("config/delete", json!({"name":remote})).await;
    }
    state.notify().await;
    if result.is_err() {
        let _ = stream
            .get_mut()
            .write_all(b"{\"error\":\"Setup failed. Check account details and retry.\"}\n")
            .await;
    }
    result
}
#[tokio::main]
async fn main() -> Result<()> {
    private_dir(&config_dir())?;
    private_dir(&mounts())?;
    if !config().exists() {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(config())?;
    }
    let runtime = runtime()?;
    let rc_path = runtime.join("rclone.sock");
    let connection = zbus::connection::Builder::session()?
        .name(BUS)?
        .internal_executor(false)
        .build()
        .await?;
    let executor_connection = connection.clone();
    tokio::spawn(async move {
        loop {
            executor_connection.executor().tick().await;
        }
    });
    // Acquiring the bus name first ensures only one service owns this socket.
    if rc_path.exists() {
        tokio::fs::remove_file(&rc_path).await?;
    }
    let mut rc_child = Command::new("rclone")
        .args(["rcd", "--rc-addr"])
        .arg(format!("unix://{}", rc_path.display()))
        .args(["--rc-no-auth", "--config"])
        .arg(config())
        .args(["--log-level", "ERROR"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Install rclone to use Cloud Storage")?;
    for _ in 0..100 {
        if rc_path.exists() {
            break;
        }
        if rc_child.try_wait()?.is_some() {
            bail!("rclone configuration service failed");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if config().exists() {
        tokio::fs::set_permissions(config(), std::fs::Permissions::from_mode(0o600)).await?;
    }
    let saved: Saved = match tokio::fs::read(config_dir().join("accounts.json")).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("Cloud account metadata is invalid; original file preserved")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
        Err(e) => return Err(e.into()),
    };
    let mut remotes = saved.remotes;
    for r in &mut remotes {
        if !id_valid(&r.id) {
            bail!("Invalid persisted cloud account identifier; metadata preserved");
        }
        r.mount_path = mounts().join(&r.id).to_string_lossy().into();
        r.connection = "disconnected".into();
        r.mount_status = "unmounted".into();
        r.error.clear();
    }
    let version_output = Command::new("rclone").arg("version").output().await?;
    if !version_output.status.success() {
        bail!("Unable to detect rclone version");
    }
    let version = String::from_utf8_lossy(&version_output.stdout)
        .lines()
        .next()
        .unwrap_or("rclone")
        .to_owned();
    let state = Arc::new(State {
        version,
        setup_gate: Mutex::new(()),
        remotes: Mutex::new(remotes),
        children: Mutex::new(HashMap::new()),
        busy: Mutex::new(HashSet::new()),
        rc: reqwest::Client::builder()
            .unix_socket(rc_path)
            .timeout(Duration::from_secs(600))
            .build()?,
        connection: connection.clone(),
        tasks: Mutex::new(HashMap::new()),
    });
    connection
        .object_server()
        .at(PATH, Cloud(state.clone()))
        .await?;
    let snapshot = state.remotes.lock().await.clone();
    for r in snapshot {
        if r.preferences.mount_on_login {
            let _ = Cloud(state.clone()).launch(&r.id, "mount").await;
        }
    }
    let health = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let snapshot = health.remotes.lock().await.clone();
            for r in snapshot {
                if r.mount_status == "mounted"
                    && r.connection != "authentication_required"
                    && !health.busy.lock().await.contains(&r.id)
                {
                    let health = health.clone();
                    tokio::spawn(async move {
                        let _ = health.test(&r.id).await;
                    });
                }
            }
        }
    });
    let monitor = state.clone();
    tokio::spawn(async move {
        let mut retries: HashMap<String, u8> = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let mut children = monitor.children.lock().await;
            let exited: Vec<_> = children
                .iter_mut()
                .filter_map(|(id, c)| c.try_wait().ok().flatten().map(|_| id.clone()))
                .collect();
            for id in &exited {
                children.remove(id);
            }
            drop(children);
            for id in exited {
                if let Ok(r) = monitor.get(&id).await {
                    if mounted(&r.mount_path).await {
                        let _ = Command::new("fusermount3")
                            .args(["-u", &r.mount_path])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .await;
                    }
                }
                monitor
                    .status(
                        &id,
                        "error",
                        "unmounted",
                        "Cloud connection stopped unexpectedly. Retry connection.",
                    )
                    .await;
                let attempts = retries.entry(id.clone()).or_default();
                if *attempts < 3 {
                    *attempts += 1;
                    if monitor.test(&id).await.is_ok() {
                        let _ = Cloud(monitor.clone()).launch(&id, "mount").await;
                    }
                }
            }
        }
    });
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = async { let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler"); sig.recv().await; } => {} }
    let snapshot = state.remotes.lock().await.clone();
    for r in snapshot {
        let _ = state.stop_mount(&r.id).await;
    }
    let _ = state.save().await;
    let _ = rc_child.kill().await;
    Ok(())
}
