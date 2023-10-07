//! Module containing structs related to membership management
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::join;
use serde::{Deserialize, Serialize};
use sodiumoxide::crypto::sign::ed25519;
use tokio::select;
use tokio::sync::watch;
use tokio::sync::Mutex;

use netapp::endpoint::{Endpoint, EndpointHandler};
use netapp::message::*;
use netapp::peering::fullmesh::FullMeshPeeringStrategy;
use netapp::util::parse_and_resolve_peer_addr_async;
use netapp::{NetApp, NetworkKey, NodeID, NodeKey};

#[cfg(feature = "kubernetes-discovery")]
use garage_util::config::KubernetesDiscoveryConfig;
use garage_util::config::{Config, DataDirEnum};
use garage_util::data::*;
use garage_util::error::*;
use garage_util::persister::Persister;
use garage_util::time::*;

#[cfg(feature = "consul-discovery")]
use crate::consul::ConsulDiscovery;
#[cfg(feature = "kubernetes-discovery")]
use crate::kubernetes::*;
use crate::layout::*;
use crate::replication_mode::*;
use crate::ring::*;
use crate::rpc_helper::*;

use crate::system_metrics::*;

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const STATUS_EXCHANGE_INTERVAL: Duration = Duration::from_secs(10);

/// Version tag used for version check upon Netapp connection.
/// Cluster nodes with different version tags are deemed
/// incompatible and will refuse to connect.
pub const GARAGE_VERSION_TAG: u64 = 0x6761726167650008; // garage 0x0008

/// RPC endpoint used for calls related to membership
pub const SYSTEM_RPC_PATH: &str = "garage_rpc/membership.rs/SystemRpc";

/// RPC messages related to membership
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum SystemRpc {
	/// Response to successfull advertisements
	Ok,
	/// Request to connect to a specific node (in <pubkey>@<host>:<port> format)
	Connect(String),
	/// Ask other node its cluster layout. Answered with AdvertiseClusterLayout
	PullClusterLayout,
	/// Advertise Garage status. Answered with another AdvertiseStatus.
	/// Exchanged with every node on a regular basis.
	AdvertiseStatus(NodeStatus),
	/// Advertisement of cluster layout. Sent spontanously or in response to PullClusterLayout
	AdvertiseClusterLayout(ClusterLayout),
	/// Get known nodes states
	GetKnownNodes,
	/// Return known nodes
	ReturnKnownNodes(Vec<KnownNodeInfo>),
}

impl Rpc for SystemRpc {
	type Response = Result<SystemRpc, Error>;
}

#[derive(Serialize, Deserialize)]
pub struct PeerList(Vec<(Uuid, SocketAddr)>);
impl garage_util::migrate::InitialFormat for PeerList {}

/// This node's membership manager
pub struct System {
	/// The id of this node
	pub id: Uuid,

	persist_cluster_layout: Persister<ClusterLayout>,
	persist_peer_list: Persister<PeerList>,

	local_status: ArcSwap<NodeStatus>,
	node_status: RwLock<HashMap<Uuid, (u64, NodeStatus)>>,

	pub netapp: Arc<NetApp>,
	fullmesh: Arc<FullMeshPeeringStrategy>,
	pub rpc: RpcHelper,

	system_endpoint: Arc<Endpoint<SystemRpc, System>>,

	rpc_listen_addr: SocketAddr,
	#[cfg(any(feature = "consul-discovery", feature = "kubernetes-discovery"))]
	rpc_public_addr: Option<SocketAddr>,
	bootstrap_peers: Vec<String>,

	#[cfg(feature = "consul-discovery")]
	consul_discovery: Option<ConsulDiscovery>,
	#[cfg(feature = "kubernetes-discovery")]
	kubernetes_discovery: Option<KubernetesDiscoveryConfig>,

	metrics: SystemMetrics,

	replication_mode: ReplicationMode,
	replication_factor: usize,

	/// The ring
	pub ring: watch::Receiver<Arc<Ring>>,
	update_ring: Mutex<watch::Sender<Arc<Ring>>>,

	/// Path to metadata directory
	pub metadata_dir: PathBuf,
	/// Path to data directory
	pub data_dir: DataDirEnum,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
	/// Hostname of the node
	pub hostname: String,

	/// Replication factor configured on the node
	pub replication_factor: usize,
	/// Cluster layout version
	pub cluster_layout_version: u64,
	/// Hash of cluster layout staging data
	pub cluster_layout_staging_hash: Hash,

	/// Disk usage on partition containing metadata directory (tuple: `(avail, total)`)
	#[serde(default)]
	pub meta_disk_avail: Option<(u64, u64)>,
	/// Disk usage on partition containing data directory (tuple: `(avail, total)`)
	#[serde(default)]
	pub data_disk_avail: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownNodeInfo {
	pub id: Uuid,
	pub addr: SocketAddr,
	pub is_up: bool,
	pub last_seen_secs_ago: Option<u64>,
	pub status: NodeStatus,
}

#[derive(Debug, Clone, Copy)]
pub struct ClusterHealth {
	/// The current health status of the cluster (see below)
	pub status: ClusterHealthStatus,
	/// Number of nodes already seen once in the cluster
	pub known_nodes: usize,
	/// Number of nodes currently connected
	pub connected_nodes: usize,
	/// Number of storage nodes declared in the current layout
	pub storage_nodes: usize,
	/// Number of storage nodes currently connected
	pub storage_nodes_ok: usize,
	/// Number of partitions in the layout
	pub partitions: usize,
	/// Number of partitions for which we have a quorum of connected nodes
	pub partitions_quorum: usize,
	/// Number of partitions for which all storage nodes are connected
	pub partitions_all_ok: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum ClusterHealthStatus {
	/// All nodes are available
	Healthy,
	/// Some storage nodes are unavailable, but quorum is stil
	/// achieved for all partitions
	Degraded,
	/// Quorum is not available for some partitions
	Unavailable,
}

pub fn read_node_id(metadata_dir: &Path) -> Result<NodeID, Error> {
	let mut pubkey_file = metadata_dir.to_path_buf();
	pubkey_file.push("node_key.pub");

	let mut f = std::fs::File::open(pubkey_file.as_path())?;
	let mut d = vec![];
	f.read_to_end(&mut d)?;
	if d.len() != 32 {
		return Err(Error::Message("Corrupt node_key.pub file".to_string()));
	}

	let mut key = [0u8; 32];
	key.copy_from_slice(&d[..]);
	Ok(NodeID::from_slice(&key[..]).unwrap())
}

pub fn gen_node_key(metadata_dir: &Path) -> Result<NodeKey, Error> {
	let mut key_file = metadata_dir.to_path_buf();
	key_file.push("node_key");
	if key_file.as_path().exists() {
		let mut f = std::fs::File::open(key_file.as_path())?;
		let mut d = vec![];
		f.read_to_end(&mut d)?;
		if d.len() != 64 {
			return Err(Error::Message("Corrupt node_key file".to_string()));
		}

		let mut key = [0u8; 64];
		key.copy_from_slice(&d[..]);
		Ok(NodeKey::from_slice(&key[..]).unwrap())
	} else {
		if !metadata_dir.exists() {
			info!("Metadata directory does not exist, creating it.");
			std::fs::create_dir(metadata_dir)?;
		}

		info!("Generating new node key pair.");
		let (pubkey, key) = ed25519::gen_keypair();

		{
			use std::os::unix::fs::PermissionsExt;
			let mut f = std::fs::File::create(key_file.as_path())?;
			let mut perm = f.metadata()?.permissions();
			perm.set_mode(0o600);
			std::fs::set_permissions(key_file.as_path(), perm)?;
			f.write_all(&key[..])?;
		}

		{
			let mut pubkey_file = metadata_dir.to_path_buf();
			pubkey_file.push("node_key.pub");
			let mut f2 = std::fs::File::create(pubkey_file.as_path())?;
			f2.write_all(&pubkey[..])?;
		}

		Ok(key)
	}
}

impl System {
	/// Create this node's membership manager
	pub fn new(
		network_key: NetworkKey,
		replication_mode: ReplicationMode,
		config: &Config,
	) -> Result<Arc<Self>, Error> {
		let replication_factor = replication_mode.replication_factor();

		let node_key =
			gen_node_key(&config.metadata_dir).expect("Unable to read or generate node ID");
		info!(
			"Node ID of this node: {}",
			hex::encode(&node_key.public_key()[..8])
		);

		let persist_cluster_layout: Persister<ClusterLayout> =
			Persister::new(&config.metadata_dir, "cluster_layout");
		let persist_peer_list = Persister::new(&config.metadata_dir, "peer_list");

		let cluster_layout = match persist_cluster_layout.load() {
			Ok(x) => {
				if x.replication_factor != replication_factor {
					return Err(Error::Message(format!(
						"Prevous cluster layout has replication factor {}, which is different than the one specified in the config file ({}). The previous cluster layout can be purged, if you know what you are doing, simply by deleting the `cluster_layout` file in your metadata directory.",
						x.replication_factor,
						replication_factor
					)));
				}
				x
			}
			Err(e) => {
				info!(
					"No valid previous cluster layout stored ({}), starting fresh.",
					e
				);
				ClusterLayout::new(replication_factor)
			}
		};

		let metrics = SystemMetrics::new(replication_factor);

		let mut local_status = NodeStatus::initial(replication_factor, &cluster_layout);
		local_status.update_disk_usage(&config.metadata_dir, &config.data_dir, &metrics);

		let ring = Ring::new(cluster_layout, replication_factor);
		let (update_ring, ring) = watch::channel(Arc::new(ring));

		let rpc_public_addr = match &config.rpc_public_addr {
			Some(a_str) => {
				use std::net::ToSocketAddrs;
				match a_str.to_socket_addrs() {
					Err(e) => {
						error!(
							"Cannot resolve rpc_public_addr {} from config file: {}.",
							a_str, e
						);
						None
					}
					Ok(a) => {
						let a = a.collect::<Vec<_>>();
						if a.is_empty() {
							error!("rpc_public_addr {} resolve to no known IP address", a_str);
						}
						if a.len() > 1 {
							warn!("Multiple possible resolutions for rpc_public_addr: {:?}. Taking the first one.", a);
						}
						a.into_iter().next()
					}
				}
			}
			None => {
				let addr =
					get_default_ip().map(|ip| SocketAddr::new(ip, config.rpc_bind_addr.port()));
				if let Some(a) = addr {
					warn!("Using autodetected rpc_public_addr: {}. Consider specifying it explicitly in configuration file if possible.", a);
				}
				addr
			}
		};
		if rpc_public_addr.is_none() {
			warn!("This Garage node does not know its publicly reachable RPC address, this might hamper intra-cluster communication.");
		}

		let netapp = NetApp::new(GARAGE_VERSION_TAG, network_key, node_key);
		let fullmesh = FullMeshPeeringStrategy::new(netapp.clone(), vec![], rpc_public_addr);
		if let Some(ping_timeout) = config.rpc_ping_timeout_msec {
			fullmesh.set_ping_timeout_millis(ping_timeout);
		}

		let system_endpoint = netapp.endpoint(SYSTEM_RPC_PATH.into());

		#[cfg(feature = "consul-discovery")]
		let consul_discovery = match &config.consul_discovery {
			Some(cfg) => Some(
				ConsulDiscovery::new(cfg.clone())
					.ok_or_message("Invalid Consul discovery configuration")?,
			),
			None => None,
		};
		#[cfg(not(feature = "consul-discovery"))]
		if config.consul_discovery.is_some() {
			warn!("Consul discovery is not enabled in this build.");
		}

		#[cfg(not(feature = "kubernetes-discovery"))]
		if config.kubernetes_discovery.is_some() {
			warn!("Kubernetes discovery is not enabled in this build.");
		}

		let sys = Arc::new(System {
			id: netapp.id.into(),
			persist_cluster_layout,
			persist_peer_list,
			local_status: ArcSwap::new(Arc::new(local_status)),
			node_status: RwLock::new(HashMap::new()),
			netapp: netapp.clone(),
			fullmesh: fullmesh.clone(),
			rpc: RpcHelper::new(
				netapp.id.into(),
				fullmesh,
				ring.clone(),
				config.rpc_timeout_msec.map(Duration::from_millis),
			),
			system_endpoint,
			replication_mode,
			replication_factor,
			rpc_listen_addr: config.rpc_bind_addr,
			#[cfg(any(feature = "consul-discovery", feature = "kubernetes-discovery"))]
			rpc_public_addr,
			bootstrap_peers: config.bootstrap_peers.clone(),
			#[cfg(feature = "consul-discovery")]
			consul_discovery,
			#[cfg(feature = "kubernetes-discovery")]
			kubernetes_discovery: config.kubernetes_discovery.clone(),
			metrics,

			ring,
			update_ring: Mutex::new(update_ring),
			metadata_dir: config.metadata_dir.clone(),
			data_dir: config.data_dir.clone(),
		});
		sys.system_endpoint.set_handler(sys.clone());
		Ok(sys)
	}

	/// Perform bootstraping, starting the ping loop
	pub async fn run(self: Arc<Self>, must_exit: watch::Receiver<bool>) {
		join!(
			self.netapp
				.clone()
				.listen(self.rpc_listen_addr, None, must_exit.clone()),
			self.fullmesh.clone().run(must_exit.clone()),
			self.discovery_loop(must_exit.clone()),
			self.status_exchange_loop(must_exit.clone()),
		);
	}

	// ---- Administrative operations (directly available and
	//      also available through RPC) ----

	pub fn get_known_nodes(&self) -> Vec<KnownNodeInfo> {
		let node_status = self.node_status.read().unwrap();
		let known_nodes = self
			.fullmesh
			.get_peer_list()
			.iter()
			.map(|n| KnownNodeInfo {
				id: n.id.into(),
				addr: n.addr,
				is_up: n.is_up(),
				last_seen_secs_ago: n
					.last_seen
					.map(|t| (Instant::now().saturating_duration_since(t)).as_secs()),
				status: node_status
					.get(&n.id.into())
					.cloned()
					.map(|(_, st)| st)
					.unwrap_or_else(NodeStatus::unknown),
			})
			.collect::<Vec<_>>();
		known_nodes
	}

	pub fn get_cluster_layout(&self) -> ClusterLayout {
		self.ring.borrow().layout.clone()
	}

	pub async fn update_cluster_layout(
		self: &Arc<Self>,
		layout: &ClusterLayout,
	) -> Result<(), Error> {
		self.handle_advertise_cluster_layout(layout).await?;
		Ok(())
	}

	pub async fn connect(&self, node: &str) -> Result<(), Error> {
		let (pubkey, addrs) = parse_and_resolve_peer_addr_async(node)
			.await
			.ok_or_else(|| {
				Error::Message(format!(
					"Unable to parse or resolve node specification: {}",
					node
				))
			})?;
		let mut errors = vec![];
		for addr in addrs.iter() {
			match self.netapp.clone().try_connect(*addr, pubkey).await {
				Ok(()) => return Ok(()),
				Err(e) => {
					errors.push((
						*addr,
						Error::Message(connect_error_message(*addr, pubkey, e)),
					));
				}
			}
		}
		if errors.len() == 1 {
			Err(Error::Message(errors[0].1.to_string()))
		} else {
			Err(Error::Message(format!("{:?}", errors)))
		}
	}

	pub fn health(&self) -> ClusterHealth {
		let ring: Arc<_> = self.ring.borrow().clone();
		let quorum = self.replication_mode.write_quorum();
		let replication_factor = self.replication_factor;

		let nodes = self
			.get_known_nodes()
			.into_iter()
			.map(|n| (n.id, n))
			.collect::<HashMap<Uuid, _>>();
		let connected_nodes = nodes.iter().filter(|(_, n)| n.is_up).count();

		let storage_nodes = ring
			.layout
			.roles
			.items()
			.iter()
			.filter(|(_, _, v)| matches!(v, NodeRoleV(Some(r)) if r.capacity.is_some()))
			.collect::<Vec<_>>();
		let storage_nodes_ok = storage_nodes
			.iter()
			.filter(|(x, _, _)| nodes.get(x).map(|n| n.is_up).unwrap_or(false))
			.count();

		let partitions = ring.partitions();
		let partitions_n_up = partitions
			.iter()
			.map(|(_, h)| {
				let pn = ring.get_nodes(h, ring.replication_factor);
				pn.iter()
					.filter(|x| nodes.get(x).map(|n| n.is_up).unwrap_or(false))
					.count()
			})
			.collect::<Vec<usize>>();
		let partitions_all_ok = partitions_n_up
			.iter()
			.filter(|c| **c == replication_factor)
			.count();
		let partitions_quorum = partitions_n_up.iter().filter(|c| **c >= quorum).count();

		let status =
			if partitions_quorum == partitions.len() && storage_nodes_ok == storage_nodes.len() {
				ClusterHealthStatus::Healthy
			} else if partitions_quorum == partitions.len() {
				ClusterHealthStatus::Degraded
			} else {
				ClusterHealthStatus::Unavailable
			};

		ClusterHealth {
			status,
			known_nodes: nodes.len(),
			connected_nodes,
			storage_nodes: storage_nodes.len(),
			storage_nodes_ok,
			partitions: partitions.len(),
			partitions_quorum,
			partitions_all_ok,
		}
	}

	// ---- INTERNALS ----

	#[cfg(feature = "consul-discovery")]
	async fn advertise_to_consul(self: Arc<Self>) {
		let c = match &self.consul_discovery {
			Some(c) => c,
			_ => return,
		};

		let rpc_public_addr = match self.rpc_public_addr {
			Some(addr) => addr,
			None => {
				warn!("Not advertising to Consul because rpc_public_addr is not defined in config file and could not be autodetected.");
				return;
			}
		};

		if let Err(e) = c
			.publish_consul_service(
				self.netapp.id,
				&self.local_status.load_full().hostname,
				rpc_public_addr,
			)
			.await
		{
			error!("Error while publishing Consul service: {}", e);
		}
	}

	#[cfg(feature = "kubernetes-discovery")]
	async fn advertise_to_kubernetes(self: Arc<Self>) {
		let k = match &self.kubernetes_discovery {
			Some(k) => k,
			_ => return,
		};

		let rpc_public_addr = match self.rpc_public_addr {
			Some(addr) => addr,
			None => {
				warn!("Not advertising to Kubernetes because rpc_public_addr is not defined in config file and could not be autodetected.");
				return;
			}
		};

		if let Err(e) = publish_kubernetes_node(
			k,
			self.netapp.id,
			&self.local_status.load_full().hostname,
			rpc_public_addr,
		)
		.await
		{
			error!("Error while publishing node to Kubernetes: {}", e);
		}
	}

	/// Save network configuration to disc
	async fn save_cluster_layout(&self) -> Result<(), Error> {
		let ring: Arc<Ring> = self.ring.borrow().clone();
		self.persist_cluster_layout
			.save_async(&ring.layout)
			.await
			.expect("Cannot save current cluster layout");
		Ok(())
	}

	fn update_local_status(&self) {
		let mut new_si: NodeStatus = self.local_status.load().as_ref().clone();

		let ring = self.ring.borrow();
		new_si.cluster_layout_version = ring.layout.version;
		new_si.cluster_layout_staging_hash = ring.layout.staging_hash;

		new_si.update_disk_usage(&self.metadata_dir, &self.data_dir, &self.metrics);

		self.local_status.swap(Arc::new(new_si));
	}

	// --- RPC HANDLERS ---

	async fn handle_connect(&self, node: &str) -> Result<SystemRpc, Error> {
		self.connect(node).await?;
		Ok(SystemRpc::Ok)
	}

	fn handle_pull_cluster_layout(&self) -> SystemRpc {
		let ring = self.ring.borrow().clone();
		SystemRpc::AdvertiseClusterLayout(ring.layout.clone())
	}

	fn handle_get_known_nodes(&self) -> SystemRpc {
		let known_nodes = self.get_known_nodes();
		SystemRpc::ReturnKnownNodes(known_nodes)
	}

	async fn handle_advertise_status(
		self: &Arc<Self>,
		from: Uuid,
		info: &NodeStatus,
	) -> Result<SystemRpc, Error> {
		let local_info = self.local_status.load();

		if local_info.replication_factor < info.replication_factor {
			error!("Some node have a higher replication factor ({}) than this one ({}). This is not supported and will lead to data corruption. Shutting down for safety.",
				info.replication_factor,
				local_info.replication_factor);
			std::process::exit(1);
		}

		if info.cluster_layout_version > local_info.cluster_layout_version
			|| info.cluster_layout_staging_hash != local_info.cluster_layout_staging_hash
		{
			tokio::spawn(self.clone().pull_cluster_layout(from));
		}

		self.node_status
			.write()
			.unwrap()
			.insert(from, (now_msec(), info.clone()));

		Ok(SystemRpc::Ok)
	}

	async fn handle_advertise_cluster_layout(
		self: &Arc<Self>,
		adv: &ClusterLayout,
	) -> Result<SystemRpc, Error> {
		if adv.replication_factor != self.replication_factor {
			let msg = format!(
				"Received a cluster layout from another node with replication factor {}, which is different from what we have in our configuration ({}). Discarding the cluster layout we received.",
				adv.replication_factor,
				self.replication_factor
			);
			error!("{}", msg);
			return Err(Error::Message(msg));
		}

		let update_ring = self.update_ring.lock().await;
		let mut layout: ClusterLayout = self.ring.borrow().layout.clone();

		let prev_layout_check = layout.check().is_ok();
		if layout.merge(adv) {
			if prev_layout_check && layout.check().is_err() {
				error!("New cluster layout is invalid, discarding.");
				return Err(Error::Message(
					"New cluster layout is invalid, discarding.".into(),
				));
			}

			let ring = Ring::new(layout.clone(), self.replication_factor);
			update_ring.send(Arc::new(ring))?;
			drop(update_ring);

			let self2 = self.clone();
			tokio::spawn(async move {
				if let Err(e) = self2
					.rpc
					.broadcast(
						&self2.system_endpoint,
						SystemRpc::AdvertiseClusterLayout(layout),
						RequestStrategy::with_priority(PRIO_HIGH),
					)
					.await
				{
					warn!("Error while broadcasting new cluster layout: {}", e);
				}
			});

			self.save_cluster_layout().await?;
		}

		Ok(SystemRpc::Ok)
	}

	async fn status_exchange_loop(&self, mut stop_signal: watch::Receiver<bool>) {
		while !*stop_signal.borrow() {
			let restart_at = Instant::now() + STATUS_EXCHANGE_INTERVAL;

			self.update_local_status();
			let local_status: NodeStatus = self.local_status.load().as_ref().clone();
			let _ = self
				.rpc
				.broadcast(
					&self.system_endpoint,
					SystemRpc::AdvertiseStatus(local_status),
					RequestStrategy::with_priority(PRIO_HIGH)
						.with_custom_timeout(STATUS_EXCHANGE_INTERVAL),
				)
				.await;

			select! {
				_ = tokio::time::sleep_until(restart_at.into()) => {},
				_ = stop_signal.changed() => {},
			}
		}
	}

	async fn discovery_loop(self: &Arc<Self>, mut stop_signal: watch::Receiver<bool>) {
		while !*stop_signal.borrow() {
			let not_configured = self.ring.borrow().layout.check().is_err();
			let no_peers = self.fullmesh.get_peer_list().len() < self.replication_factor;
			let expected_n_nodes = self.ring.borrow().layout.num_nodes();
			let bad_peers = self
				.fullmesh
				.get_peer_list()
				.iter()
				.filter(|p| p.is_up())
				.count() != expected_n_nodes;

			if not_configured || no_peers || bad_peers {
				info!("Doing a bootstrap/discovery step (not_configured: {}, no_peers: {}, bad_peers: {})", not_configured, no_peers, bad_peers);

				let mut ping_list = resolve_peers(&self.bootstrap_peers).await;

				// Add peer list from list stored on disk
				if let Ok(peers) = self.persist_peer_list.load_async().await {
					ping_list.extend(peers.0.iter().map(|(id, addr)| ((*id).into(), *addr)))
				}

				// Fetch peer list from Consul
				#[cfg(feature = "consul-discovery")]
				if let Some(c) = &self.consul_discovery {
					match c.get_consul_nodes().await {
						Ok(node_list) => {
							ping_list.extend(node_list);
						}
						Err(e) => {
							warn!("Could not retrieve node list from Consul: {}", e);
						}
					}
				}

				// Fetch peer list from Kubernetes
				#[cfg(feature = "kubernetes-discovery")]
				if let Some(k) = &self.kubernetes_discovery {
					if !k.skip_crd {
						match create_kubernetes_crd().await {
							Ok(()) => (),
							Err(e) => {
								error!("Failed to create kubernetes custom resource: {}", e)
							}
						};
					}

					match get_kubernetes_nodes(k).await {
						Ok(node_list) => {
							ping_list.extend(node_list);
						}
						Err(e) => {
							warn!("Could not retrieve node list from Kubernetes: {}", e);
						}
					}
				}

				for (node_id, node_addr) in ping_list {
					let self2 = self.clone();
					tokio::spawn(async move {
						if let Err(e) = self2.netapp.clone().try_connect(node_addr, node_id).await {
							error!("{}", connect_error_message(node_addr, node_id, e));
						}
					});
				}
			}

			if let Err(e) = self.save_peer_list().await {
				warn!("Could not save peer list to file: {}", e);
			}

			#[cfg(feature = "consul-discovery")]
			tokio::spawn(self.clone().advertise_to_consul());

			#[cfg(feature = "kubernetes-discovery")]
			tokio::spawn(self.clone().advertise_to_kubernetes());

			select! {
				_ = tokio::time::sleep(DISCOVERY_INTERVAL) => {},
				_ = stop_signal.changed() => {},
			}
		}
	}

	async fn save_peer_list(&self) -> Result<(), Error> {
		// Prepare new peer list to save to file
		// It is a vec of tuples (node ID as Uuid, node SocketAddr)
		let mut peer_list = self
			.fullmesh
			.get_peer_list()
			.iter()
			.map(|n| (n.id.into(), n.addr))
			.collect::<Vec<_>>();

		// Before doing it, we read the current peer list file (if it exists)
		// and append it to the list we are about to save,
		// so that no peer ID gets lost in the process.
		if let Ok(mut prev_peer_list) = self.persist_peer_list.load_async().await {
			prev_peer_list
				.0
				.retain(|(id, _ip)| peer_list.iter().all(|(id2, _ip2)| id2 != id));
			peer_list.extend(prev_peer_list.0);
		}

		// Save new peer list to file
		self.persist_peer_list
			.save_async(&PeerList(peer_list))
			.await
	}

	async fn pull_cluster_layout(self: Arc<Self>, peer: Uuid) {
		let resp = self
			.rpc
			.call(
				&self.system_endpoint,
				peer,
				SystemRpc::PullClusterLayout,
				RequestStrategy::with_priority(PRIO_HIGH),
			)
			.await;
		if let Ok(SystemRpc::AdvertiseClusterLayout(layout)) = resp {
			let _: Result<_, _> = self.handle_advertise_cluster_layout(&layout).await;
		}
	}
}

#[async_trait]
impl EndpointHandler<SystemRpc> for System {
	async fn handle(self: &Arc<Self>, msg: &SystemRpc, from: NodeID) -> Result<SystemRpc, Error> {
		match msg {
			SystemRpc::Connect(node) => self.handle_connect(node).await,
			SystemRpc::PullClusterLayout => Ok(self.handle_pull_cluster_layout()),
			SystemRpc::AdvertiseStatus(adv) => self.handle_advertise_status(from.into(), adv).await,
			SystemRpc::AdvertiseClusterLayout(adv) => {
				self.clone().handle_advertise_cluster_layout(adv).await
			}
			SystemRpc::GetKnownNodes => Ok(self.handle_get_known_nodes()),
			m => Err(Error::unexpected_rpc_message(m)),
		}
	}
}

impl NodeStatus {
	fn initial(replication_factor: usize, layout: &ClusterLayout) -> Self {
		NodeStatus {
			hostname: gethostname::gethostname()
				.into_string()
				.unwrap_or_else(|_| "<invalid utf-8>".to_string()),
			replication_factor,
			cluster_layout_version: layout.version,
			cluster_layout_staging_hash: layout.staging_hash,
			meta_disk_avail: None,
			data_disk_avail: None,
		}
	}

	fn unknown() -> Self {
		NodeStatus {
			hostname: "?".to_string(),
			replication_factor: 0,
			cluster_layout_version: 0,
			cluster_layout_staging_hash: Hash::from([0u8; 32]),
			meta_disk_avail: None,
			data_disk_avail: None,
		}
	}

	fn update_disk_usage(
		&mut self,
		meta_dir: &Path,
		data_dir: &DataDirEnum,
		metrics: &SystemMetrics,
	) {
		use nix::sys::statvfs::statvfs;
		let mount_avail = |path: &Path| match statvfs(path) {
			Ok(x) => {
				let avail = x.blocks_available() * x.fragment_size() as u64;
				let total = x.blocks() * x.fragment_size() as u64;
				Some((x.filesystem_id(), avail, total))
			}
			Err(_) => None,
		};

		self.meta_disk_avail = mount_avail(meta_dir).map(|(_, a, t)| (a, t));
		self.data_disk_avail = match data_dir {
			DataDirEnum::Single(dir) => mount_avail(dir).map(|(_, a, t)| (a, t)),
			DataDirEnum::Multiple(dirs) => (|| {
				// TODO: more precise calculation that takes into account
				// how data is going to be spread among partitions
				let mut mounts = HashMap::new();
				for dir in dirs.iter() {
					if dir.capacity.is_none() {
						continue;
					}
					match mount_avail(&dir.path) {
						Some((fsid, avail, total)) => {
							mounts.insert(fsid, (avail, total));
						}
						None => return None,
					}
				}
				Some(
					mounts
						.into_iter()
						.fold((0, 0), |(x, y), (_, (a, b))| (x + a, y + b)),
				)
			})(),
		};

		if let Some((avail, total)) = self.meta_disk_avail {
			metrics
				.values
				.meta_disk_avail
				.store(avail, Ordering::Relaxed);
			metrics
				.values
				.meta_disk_total
				.store(total, Ordering::Relaxed);
		}
		if let Some((avail, total)) = self.data_disk_avail {
			metrics
				.values
				.data_disk_avail
				.store(avail, Ordering::Relaxed);
			metrics
				.values
				.data_disk_total
				.store(total, Ordering::Relaxed);
		}
	}
}

fn get_default_ip() -> Option<IpAddr> {
	pnet_datalink::interfaces()
		.iter()
		.find(|e| e.is_up() && !e.is_loopback() && !e.ips.is_empty())
		.and_then(|e| e.ips.first())
		.map(|a| a.ip())
}

async fn resolve_peers(peers: &[String]) -> Vec<(NodeID, SocketAddr)> {
	let mut ret = vec![];

	for peer in peers.iter() {
		match parse_and_resolve_peer_addr_async(peer).await {
			Some((pubkey, addrs)) => {
				for ip in addrs {
					ret.push((pubkey, ip));
				}
			}
			None => {
				warn!("Unable to parse and/or resolve peer hostname {}", peer);
			}
		}
	}

	ret
}

fn connect_error_message(
	addr: SocketAddr,
	pubkey: ed25519::PublicKey,
	e: netapp::error::Error,
) -> String {
	format!("Error establishing RPC connection to remote node: {}@{}.\nThis can happen if the remote node is not reachable on the network, but also if the two nodes are not configured with the same rpc_secret.\n{}", hex::encode(pubkey), addr, e)
}
