use std::sync::Arc;

use netapp::NetworkKey;

use garage_db as db;

use garage_util::background::*;
use garage_util::config::*;
use garage_util::error::*;
use garage_util::persister::PersisterShared;

use garage_rpc::replication_mode::ReplicationMode;
use garage_rpc::system::System;

use garage_block::manager::*;
use garage_table::replication::TableFullReplication;
use garage_table::replication::TableShardedReplication;
use garage_table::*;

use crate::s3::block_ref_table::*;
use crate::s3::lifecycle_worker;
use crate::s3::mpu_table::*;
use crate::s3::object_table::*;
use crate::s3::version_table::*;

use crate::bucket_alias_table::*;
use crate::bucket_table::*;
use crate::helper;
use crate::index_counter::*;
use crate::key_table::*;

#[cfg(feature = "k2v")]
use crate::k2v::{item_table::*, rpc::*, sub::*};

/// An entire Garage full of data
pub struct Garage {
	/// The parsed configuration Garage is running
	pub config: Config,
	/// The set of background variables that can be viewed/modified at runtime
	pub bg_vars: vars::BgVars,

	/// The replication mode of this cluster
	pub replication_mode: ReplicationMode,

	/// The local database
	pub db: db::Db,
	/// The membership manager
	pub system: Arc<System>,
	/// The block manager
	pub block_manager: Arc<BlockManager>,

	/// Table containing buckets
	pub bucket_table: Arc<Table<BucketTable, TableFullReplication>>,
	/// Table containing bucket aliases
	pub bucket_alias_table: Arc<Table<BucketAliasTable, TableFullReplication>>,
	/// Table containing api keys
	pub key_table: Arc<Table<KeyTable, TableFullReplication>>,

	/// Table containing S3 objects
	pub object_table: Arc<Table<ObjectTable, TableShardedReplication>>,
	/// Counting table containing object counters
	pub object_counter_table: Arc<IndexCounter<Object>>,
	/// Table containing S3 multipart uploads
	pub mpu_table: Arc<Table<MultipartUploadTable, TableShardedReplication>>,
	/// Counting table containing multipart object counters
	pub mpu_counter_table: Arc<IndexCounter<MultipartUpload>>,
	/// Table containing S3 object versions
	pub version_table: Arc<Table<VersionTable, TableShardedReplication>>,
	/// Table containing S3 block references (not blocks themselves)
	pub block_ref_table: Arc<Table<BlockRefTable, TableShardedReplication>>,

	/// Persister for lifecycle worker info
	pub lifecycle_persister: PersisterShared<lifecycle_worker::LifecycleWorkerPersisted>,

	#[cfg(feature = "k2v")]
	pub k2v: GarageK2V,
}

#[cfg(feature = "k2v")]
pub struct GarageK2V {
	/// Table containing K2V items
	pub item_table: Arc<Table<K2VItemTable, TableShardedReplication>>,
	/// Indexing table containing K2V item counters
	pub counter_table: Arc<IndexCounter<K2VItem>>,
	/// K2V RPC handler
	pub rpc: Arc<K2VRpcHandler>,
}

impl Garage {
	/// Create and run garage
	pub fn new(config: Config) -> Result<Arc<Self>, Error> {
		// Create meta dir and data dir if they don't exist already
		std::fs::create_dir_all(&config.metadata_dir)
			.ok_or_message("Unable to create Garage metadata directory")?;
		match &config.data_dir {
			DataDirEnum::Single(data_dir) => {
				std::fs::create_dir_all(data_dir).ok_or_message(format!(
					"Unable to create Garage data directory: {}",
					data_dir.to_string_lossy()
				))?;
			}
			DataDirEnum::Multiple(data_dirs) => {
				for dir in data_dirs {
					std::fs::create_dir_all(&dir.path).ok_or_message(format!(
						"Unable to create Garage data directory: {}",
						dir.path.to_string_lossy()
					))?;
				}
			}
		}

		info!("Opening database...");
		let mut db_path = config.metadata_dir.clone();
		let db = match config.db_engine.as_str() {
			// ---- Sled DB ----
			#[cfg(feature = "sled")]
			"sled" => {
				if config.metadata_fsync {
					return Err(Error::Message(format!(
						"`metadata_fsync = true` is not supported with the Sled database engine"
					)));
				}
				db_path.push("db");
				info!("Opening Sled database at: {}", db_path.display());
				let db = db::sled_adapter::sled::Config::default()
					.path(&db_path)
					.cache_capacity(config.sled_cache_capacity as u64)
					.flush_every_ms(Some(config.sled_flush_every_ms))
					.open()
					.ok_or_message("Unable to open sled DB")?;
				db::sled_adapter::SledDb::init(db)
			}
			#[cfg(not(feature = "sled"))]
			"sled" => return Err(Error::Message("sled db not available in this build".into())),
			// ---- Sqlite DB ----
			#[cfg(feature = "sqlite")]
			"sqlite" | "sqlite3" | "rusqlite" => {
				db_path.push("db.sqlite");
				info!("Opening Sqlite database at: {}", db_path.display());
				let db = db::sqlite_adapter::rusqlite::Connection::open(db_path)
					.and_then(|db| {
						db.pragma_update(None, "journal_mode", &"WAL")?;
						if config.metadata_fsync {
							db.pragma_update(None, "synchronous", &"NORMAL")?;
						} else {
							db.pragma_update(None, "synchronous", &"OFF")?;
						}
						Ok(db)
					})
					.ok_or_message("Unable to open sqlite DB")?;
				db::sqlite_adapter::SqliteDb::init(db)
			}
			#[cfg(not(feature = "sqlite"))]
			"sqlite" | "sqlite3" | "rusqlite" => {
				return Err(Error::Message(
					"sqlite db not available in this build".into(),
				))
			}
			// ---- LMDB DB ----
			#[cfg(feature = "lmdb")]
			"lmdb" | "heed" => {
				db_path.push("db.lmdb");
				info!("Opening LMDB database at: {}", db_path.display());
				std::fs::create_dir_all(&db_path)
					.ok_or_message("Unable to create LMDB data directory")?;
				let map_size = match config.lmdb_map_size {
					v if v == usize::default() => garage_db::lmdb_adapter::recommended_map_size(),
					v => v - (v % 4096),
				};

				use db::lmdb_adapter::heed;
				let mut env_builder = heed::EnvOpenOptions::new();
				env_builder.max_dbs(100);
				env_builder.max_readers(500);
				env_builder.map_size(map_size);
				unsafe {
					env_builder.flag(heed::flags::Flags::MdbNoMetaSync);
					if !config.metadata_fsync {
						env_builder.flag(heed::flags::Flags::MdbNoSync);
					}
				}
				let db = match env_builder.open(&db_path) {
					Err(heed::Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory => {
						return Err(Error::Message(
							"OutOfMemory error while trying to open LMDB database. This can happen \
							if your operating system is not allowing you to use sufficient virtual \
							memory address space. Please check that no limit is set (ulimit -v). \
							You may also try to set a smaller `lmdb_map_size` configuration parameter. \
							On 32-bit machines, you should probably switch to another database engine.".into()))
					}
					x => x.ok_or_message("Unable to open LMDB DB")?,
				};
				db::lmdb_adapter::LmdbDb::init(db)
			}
			#[cfg(not(feature = "lmdb"))]
			"lmdb" | "heed" => return Err(Error::Message("lmdb db not available in this build".into())),
			// ---- Unavailable DB engine ----
			e => {
				return Err(Error::Message(format!(
					"Unsupported DB engine: {} (options: {})",
					e,
					vec![
						#[cfg(feature = "sled")]
						"sled",
						#[cfg(feature = "sqlite")]
						"sqlite",
						#[cfg(feature = "lmdb")]
						"lmdb",
					]
					.join(", ")
				)));
			}
		};

		let network_key = hex::decode(config.rpc_secret.as_ref().ok_or_message(
			"rpc_secret value is missing, not present in config file or in environment",
		)?)
		.ok()
		.and_then(|x| NetworkKey::from_slice(&x))
		.ok_or_message("Invalid RPC secret key")?;

		let replication_mode = ReplicationMode::parse(&config.replication_mode)
			.ok_or_message("Invalid replication_mode in config file.")?;

		info!("Initialize background variable system...");
		let mut bg_vars = vars::BgVars::new();

		info!("Initialize membership management system...");
		let system = System::new(network_key, replication_mode, &config)?;

		let data_rep_param = TableShardedReplication {
			system: system.clone(),
			replication_factor: replication_mode.replication_factor(),
			write_quorum: replication_mode.write_quorum(),
			read_quorum: 1,
		};

		let meta_rep_param = TableShardedReplication {
			system: system.clone(),
			replication_factor: replication_mode.replication_factor(),
			write_quorum: replication_mode.write_quorum(),
			read_quorum: replication_mode.read_quorum(),
		};

		let control_rep_param = TableFullReplication {
			system: system.clone(),
			max_faults: replication_mode.control_write_max_faults(),
		};

		info!("Initialize block manager...");
		let block_manager = BlockManager::new(
			&db,
			config.data_dir.clone(),
			config.data_fsync,
			config.compression_level,
			data_rep_param,
			system.clone(),
		)?;
		block_manager.register_bg_vars(&mut bg_vars);

		// ---- admin tables ----
		info!("Initialize bucket_table...");
		let bucket_table = Table::new(BucketTable, control_rep_param.clone(), system.clone(), &db);

		info!("Initialize bucket_alias_table...");
		let bucket_alias_table = Table::new(
			BucketAliasTable,
			control_rep_param.clone(),
			system.clone(),
			&db,
		);
		info!("Initialize key_table_table...");
		let key_table = Table::new(KeyTable, control_rep_param, system.clone(), &db);

		// ---- S3 tables ----
		info!("Initialize block_ref_table...");
		let block_ref_table = Table::new(
			BlockRefTable {
				block_manager: block_manager.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
		);

		info!("Initialize version_table...");
		let version_table = Table::new(
			VersionTable {
				block_ref_table: block_ref_table.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
		);

		info!("Initialize multipart upload counter table...");
		let mpu_counter_table = IndexCounter::new(system.clone(), meta_rep_param.clone(), &db);

		info!("Initialize multipart upload table...");
		let mpu_table = Table::new(
			MultipartUploadTable {
				version_table: version_table.clone(),
				mpu_counter_table: mpu_counter_table.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
		);

		info!("Initialize object counter table...");
		let object_counter_table = IndexCounter::new(system.clone(), meta_rep_param.clone(), &db);

		info!("Initialize object_table...");
		#[allow(clippy::redundant_clone)]
		let object_table = Table::new(
			ObjectTable {
				version_table: version_table.clone(),
				mpu_table: mpu_table.clone(),
				object_counter_table: object_counter_table.clone(),
			},
			meta_rep_param.clone(),
			system.clone(),
			&db,
		);

		info!("Load lifecycle worker state...");
		let lifecycle_persister =
			PersisterShared::new(&system.metadata_dir, "lifecycle_worker_state");
		lifecycle_worker::register_bg_vars(&lifecycle_persister, &mut bg_vars);

		// ---- K2V ----
		#[cfg(feature = "k2v")]
		let k2v = GarageK2V::new(system.clone(), &db, meta_rep_param);

		// -- done --
		Ok(Arc::new(Self {
			config,
			bg_vars,
			replication_mode,
			db,
			system,
			block_manager,
			bucket_table,
			bucket_alias_table,
			key_table,
			object_table,
			object_counter_table,
			mpu_table,
			mpu_counter_table,
			version_table,
			block_ref_table,
			lifecycle_persister,
			#[cfg(feature = "k2v")]
			k2v,
		}))
	}

	pub fn spawn_workers(self: &Arc<Self>, bg: &BackgroundRunner) {
		self.block_manager.spawn_workers(bg);

		self.bucket_table.spawn_workers(bg);
		self.bucket_alias_table.spawn_workers(bg);
		self.key_table.spawn_workers(bg);

		self.object_table.spawn_workers(bg);
		self.object_counter_table.spawn_workers(bg);
		self.mpu_table.spawn_workers(bg);
		self.mpu_counter_table.spawn_workers(bg);
		self.version_table.spawn_workers(bg);
		self.block_ref_table.spawn_workers(bg);

		bg.spawn_worker(lifecycle_worker::LifecycleWorker::new(
			self.clone(),
			self.lifecycle_persister.clone(),
		));

		#[cfg(feature = "k2v")]
		self.k2v.spawn_workers(bg);
	}

	pub fn bucket_helper(&self) -> helper::bucket::BucketHelper {
		helper::bucket::BucketHelper(self)
	}

	pub fn key_helper(&self) -> helper::key::KeyHelper {
		helper::key::KeyHelper(self)
	}
}

#[cfg(feature = "k2v")]
impl GarageK2V {
	fn new(system: Arc<System>, db: &db::Db, meta_rep_param: TableShardedReplication) -> Self {
		info!("Initialize K2V counter table...");
		let counter_table = IndexCounter::new(system.clone(), meta_rep_param.clone(), db);

		info!("Initialize K2V subscription manager...");
		let subscriptions = Arc::new(SubscriptionManager::new());

		info!("Initialize K2V item table...");
		let item_table = Table::new(
			K2VItemTable {
				counter_table: counter_table.clone(),
				subscriptions: subscriptions.clone(),
			},
			meta_rep_param,
			system.clone(),
			db,
		);

		info!("Initialize K2V RPC handler...");
		let rpc = K2VRpcHandler::new(system, db, item_table.clone(), subscriptions);

		Self {
			item_table,
			counter_table,
			rpc,
		}
	}

	pub fn spawn_workers(&self, bg: &BackgroundRunner) {
		self.item_table.spawn_workers(bg);
		self.counter_table.spawn_workers(bg);
	}
}
