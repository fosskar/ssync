//! Shared multi-node test harness: scratch dirs, identities, node spawning,
//! engines, and the tick-poll loop live here once — tests keep only their
//! scenario. Node wiring (namespace choice, tickets, auto-download) stays
//! explicit per test: that variance IS the scenario.

// not every test target uses every helper
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use ssync_adapters::Adapter;
use ssync_adapters::pi::PiAdapter;
use ssync_core::Engine;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh::SecretKey;

/// One test's world: a scratch dir plus the shared age identity.
pub struct Sim {
    pub base: PathBuf,
    /// Shared identity secret; per-machine-key tests build their own.
    pub secret: String,
}

impl Sim {
    pub fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ssync-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        Self {
            base,
            secret: AgeIdentity::generate().unwrap().to_secret_string(),
        }
    }

    /// The shared identity (a fresh instance per call — `Engine` takes it by value).
    pub fn identity(&self) -> AgeIdentity {
        AgeIdentity::from_secret_string(&self.secret).unwrap()
    }

    /// Spawn `name`'s node at `base/{name}/data` with a fresh key.
    pub async fn node(&self, name: &str) -> Node {
        self.node_with_key(name, SecretKey::generate()).await
    }

    /// Same, with a caller-held key (restart tests reuse it and the data dir).
    pub async fn node_with_key(&self, name: &str, key: SecretKey) -> Node {
        Node::spawn(&self.base.join(name).join("data"), key)
            .await
            .unwrap()
    }

    /// `name`'s session root at `base/{name}/sessions`, created.
    pub fn root(&self, name: &str) -> PathBuf {
        let root = self.base.join(name).join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A peer running one pi-layout agent on the shared identity.
    pub fn pi_peer(&self, name: &str, agent: &str, node: Node) -> Peer {
        self.pi_peer_as(name, agent, self.identity(), node)
    }

    /// Same, with a per-machine identity (multi-recipient tests).
    pub fn pi_peer_as(&self, name: &str, agent: &str, identity: AgeIdentity, node: Node) -> Peer {
        let root = self.root(name);
        self.peer(
            name,
            vec![Box::new(PiAdapter::new(agent, &root))],
            identity,
            node,
        )
    }

    /// Fully custom peer: any adapters, any identity. `root` stays the
    /// convention dir; multi-root tests address their files absolutely.
    pub fn peer(
        &self,
        name: &str,
        adapters: Vec<Box<dyn Adapter>>,
        identity: AgeIdentity,
        node: Node,
    ) -> Peer {
        Peer {
            root: self.root(name),
            dir: self.base.join(name),
            engine: Engine::with_adapters(adapters, identity, node),
        }
    }
}

/// One machine: its session root, its per-machine dir, its engine.
pub struct Peer {
    pub root: PathBuf,
    /// `base/{name}` — status and state files live here.
    pub dir: PathBuf,
    pub engine: Engine,
}

impl Peer {
    pub fn path(&self, rel: impl AsRef<str>) -> PathBuf {
        self.root.join(rel.as_ref())
    }

    /// mkdir -p + write under the session root.
    pub fn write(&self, rel: impl AsRef<str>, bytes: impl AsRef<[u8]>) {
        write_file(&self.root.join(rel.as_ref()), bytes.as_ref());
    }

    pub async fn tick(&mut self) {
        self.engine.tick_once().await;
    }

    /// Persist engine state at `dir/state.toml` — stable across restarts.
    pub fn persist(&mut self) {
        let path = self.dir.join("state.toml");
        self.engine.persist_state(&path);
    }

    /// Hand the engine to the daemon loop (status at `dir/status.toml`);
    /// returns the session root, the only handle tests still need.
    pub fn run(self) -> PathBuf {
        let status = self.dir.join("status.toml");
        let mut engine = self.engine;
        tokio::spawn(async move { engine.run(&status).await });
        self.root
    }

    /// Shut down the engine's node (restart tests re-spawn on the same dir).
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.engine.shutdown().await
    }
}

/// mkdir -p + write.
pub fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Whether `path` currently holds exactly `bytes`.
pub fn file_eq(path: &Path, bytes: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|got| got == bytes)
}

/// Poll `cond` every 500ms until it holds or ~30s elapse.
pub async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..60 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Tick every peer, then check `cond`; repeat every 500ms up to ~30s.
/// The production tick path is the only mutation, as everywhere.
pub async fn converge(peers: &mut [&mut Peer], mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..60 {
        for p in peers.iter_mut() {
            p.engine.tick_once().await;
        }
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}
