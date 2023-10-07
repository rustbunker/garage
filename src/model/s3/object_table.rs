use serde::{Deserialize, Serialize};
use std::sync::Arc;

use garage_db as db;

use garage_util::data::*;

use garage_table::crdt::*;
use garage_table::replication::TableShardedReplication;
use garage_table::*;

use crate::index_counter::*;
use crate::s3::mpu_table::*;
use crate::s3::version_table::*;

pub const OBJECTS: &str = "objects";
pub const UNFINISHED_UPLOADS: &str = "unfinished_uploads";
pub const BYTES: &str = "bytes";

mod v05 {
	use garage_util::data::{Hash, Uuid};
	use serde::{Deserialize, Serialize};
	use std::collections::BTreeMap;

	/// An object
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub struct Object {
		/// The bucket in which the object is stored, used as partition key
		pub bucket: String,

		/// The key at which the object is stored in its bucket, used as sorting key
		pub key: String,

		/// The list of currenty stored versions of the object
		pub(super) versions: Vec<ObjectVersion>,
	}

	/// Informations about a version of an object
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub struct ObjectVersion {
		/// Id of the version
		pub uuid: Uuid,
		/// Timestamp of when the object was created
		pub timestamp: u64,
		/// State of the version
		pub state: ObjectVersionState,
	}

	/// State of an object version
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub enum ObjectVersionState {
		/// The version is being received
		Uploading(ObjectVersionHeaders),
		/// The version is fully received
		Complete(ObjectVersionData),
		/// The version uploaded containded errors or the upload was explicitly aborted
		Aborted,
	}

	/// Data stored in object version
	#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
	pub enum ObjectVersionData {
		/// The object was deleted, this Version is a tombstone to mark it as such
		DeleteMarker,
		/// The object is short, it's stored inlined
		Inline(ObjectVersionMeta, #[serde(with = "serde_bytes")] Vec<u8>),
		/// The object is not short, Hash of first block is stored here, next segments hashes are
		/// stored in the version table
		FirstBlock(ObjectVersionMeta, Hash),
	}

	/// Metadata about the object version
	#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
	pub struct ObjectVersionMeta {
		/// Headers to send to the client
		pub headers: ObjectVersionHeaders,
		/// Size of the object
		pub size: u64,
		/// etag of the object
		pub etag: String,
	}

	/// Additional headers for an object
	#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
	pub struct ObjectVersionHeaders {
		/// Content type of the object
		pub content_type: String,
		/// Any other http headers to send
		pub other: BTreeMap<String, String>,
	}

	impl garage_util::migrate::InitialFormat for Object {}
}

mod v08 {
	use garage_util::data::Uuid;
	use serde::{Deserialize, Serialize};

	use super::v05;

	pub use v05::{
		ObjectVersion, ObjectVersionData, ObjectVersionHeaders, ObjectVersionMeta,
		ObjectVersionState,
	};

	/// An object
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub struct Object {
		/// The bucket in which the object is stored, used as partition key
		pub bucket_id: Uuid,

		/// The key at which the object is stored in its bucket, used as sorting key
		pub key: String,

		/// The list of currenty stored versions of the object
		pub(super) versions: Vec<ObjectVersion>,
	}

	impl garage_util::migrate::Migrate for Object {
		type Previous = v05::Object;

		fn migrate(old: v05::Object) -> Object {
			use garage_util::data::blake2sum;

			Object {
				bucket_id: blake2sum(old.bucket.as_bytes()),
				key: old.key,
				versions: old.versions,
			}
		}
	}
}

mod v09 {
	use garage_util::data::Uuid;
	use serde::{Deserialize, Serialize};

	use super::v08;

	pub use v08::{ObjectVersionData, ObjectVersionHeaders, ObjectVersionMeta};

	/// An object
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub struct Object {
		/// The bucket in which the object is stored, used as partition key
		pub bucket_id: Uuid,

		/// The key at which the object is stored in its bucket, used as sorting key
		pub key: String,

		/// The list of currenty stored versions of the object
		pub(super) versions: Vec<ObjectVersion>,
	}

	/// Informations about a version of an object
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub struct ObjectVersion {
		/// Id of the version
		pub uuid: Uuid,
		/// Timestamp of when the object was created
		pub timestamp: u64,
		/// State of the version
		pub state: ObjectVersionState,
	}

	/// State of an object version
	#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
	pub enum ObjectVersionState {
		/// The version is being received
		Uploading {
			/// Indicates whether this is a multipart upload
			multipart: bool,
			/// Headers to be included in the final object
			headers: ObjectVersionHeaders,
		},
		/// The version is fully received
		Complete(ObjectVersionData),
		/// The version uploaded containded errors or the upload was explicitly aborted
		Aborted,
	}

	impl garage_util::migrate::Migrate for Object {
		const VERSION_MARKER: &'static [u8] = b"G09s3o";

		type Previous = v08::Object;

		fn migrate(old: v08::Object) -> Object {
			let versions = old
				.versions
				.into_iter()
				.map(|x| ObjectVersion {
					uuid: x.uuid,
					timestamp: x.timestamp,
					state: match x.state {
						v08::ObjectVersionState::Uploading(h) => ObjectVersionState::Uploading {
							multipart: false,
							headers: h,
						},
						v08::ObjectVersionState::Complete(d) => ObjectVersionState::Complete(d),
						v08::ObjectVersionState::Aborted => ObjectVersionState::Aborted,
					},
				})
				.collect();
			Object {
				bucket_id: old.bucket_id,
				key: old.key,
				versions,
			}
		}
	}
}

pub use v09::*;

impl Object {
	/// Initialize an Object struct from parts
	pub fn new(bucket_id: Uuid, key: String, versions: Vec<ObjectVersion>) -> Self {
		let mut ret = Self {
			bucket_id,
			key,
			versions: vec![],
		};
		for v in versions {
			ret.add_version(v)
				.expect("Twice the same ObjectVersion in Object constructor");
		}
		ret
	}

	/// Adds a version if it wasn't already present
	#[allow(clippy::result_unit_err)]
	pub fn add_version(&mut self, new: ObjectVersion) -> Result<(), ()> {
		match self
			.versions
			.binary_search_by(|v| v.cmp_key().cmp(&new.cmp_key()))
		{
			Err(i) => {
				self.versions.insert(i, new);
				Ok(())
			}
			Ok(_) => Err(()),
		}
	}

	/// Get a list of currently stored versions of `Object`
	pub fn versions(&self) -> &[ObjectVersion] {
		&self.versions[..]
	}
}

impl Crdt for ObjectVersionState {
	fn merge(&mut self, other: &Self) {
		use ObjectVersionState::*;
		match other {
			Aborted => {
				*self = Aborted;
			}
			Complete(b) => match self {
				Aborted => {}
				Complete(a) => {
					a.merge(b);
				}
				Uploading { .. } => {
					*self = Complete(b.clone());
				}
			},
			Uploading { .. } => {}
		}
	}
}

impl AutoCrdt for ObjectVersionData {
	const WARN_IF_DIFFERENT: bool = true;
}

impl ObjectVersion {
	fn cmp_key(&self) -> (u64, Uuid) {
		(self.timestamp, self.uuid)
	}

	/// Is the object version currently being uploaded
	///
	/// matches only multipart uploads if check_multipart is Some(true)
	/// matches only non-multipart uploads if check_multipart is Some(false)
	/// matches both if check_multipart is None
	pub fn is_uploading(&self, check_multipart: Option<bool>) -> bool {
		match &self.state {
			ObjectVersionState::Uploading { multipart, .. } => {
				check_multipart.map(|x| x == *multipart).unwrap_or(true)
			}
			_ => false,
		}
	}

	/// Is the object version completely received
	pub fn is_complete(&self) -> bool {
		matches!(self.state, ObjectVersionState::Complete(_))
	}

	/// Is the object version available (received and not a tombstone)
	pub fn is_data(&self) -> bool {
		match self.state {
			ObjectVersionState::Complete(ObjectVersionData::DeleteMarker) => false,
			ObjectVersionState::Complete(_) => true,
			_ => false,
		}
	}
}

impl Entry<Uuid, String> for Object {
	fn partition_key(&self) -> &Uuid {
		&self.bucket_id
	}
	fn sort_key(&self) -> &String {
		&self.key
	}
	fn is_tombstone(&self) -> bool {
		self.versions.len() == 1
			&& self.versions[0].state
				== ObjectVersionState::Complete(ObjectVersionData::DeleteMarker)
	}
}

impl Crdt for Object {
	fn merge(&mut self, other: &Self) {
		// Merge versions from other into here
		for other_v in other.versions.iter() {
			match self
				.versions
				.binary_search_by(|v| v.cmp_key().cmp(&other_v.cmp_key()))
			{
				Ok(i) => {
					self.versions[i].state.merge(&other_v.state);
				}
				Err(i) => {
					self.versions.insert(i, other_v.clone());
				}
			}
		}

		// Remove versions which are obsolete, i.e. those that come
		// before the last version which .is_complete().
		let last_complete = self
			.versions
			.iter()
			.enumerate()
			.rev()
			.find(|(_, v)| v.is_complete())
			.map(|(vi, _)| vi);

		if let Some(last_vi) = last_complete {
			self.versions = self.versions.drain(last_vi..).collect::<Vec<_>>();
		}
	}
}

pub struct ObjectTable {
	pub version_table: Arc<Table<VersionTable, TableShardedReplication>>,
	pub mpu_table: Arc<Table<MultipartUploadTable, TableShardedReplication>>,
	pub object_counter_table: Arc<IndexCounter<Object>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ObjectFilter {
	/// Is the object version available (received and not a tombstone)
	IsData,
	/// Is the object version currently being uploaded
	///
	/// matches only multipart uploads if check_multipart is Some(true)
	/// matches only non-multipart uploads if check_multipart is Some(false)
	/// matches both if check_multipart is None
	IsUploading { check_multipart: Option<bool> },
}

impl TableSchema for ObjectTable {
	const TABLE_NAME: &'static str = "object";

	type P = Uuid;
	type S = String;
	type E = Object;
	type Filter = ObjectFilter;

	fn updated(
		&self,
		tx: &mut db::Transaction,
		old: Option<&Self::E>,
		new: Option<&Self::E>,
	) -> db::TxOpResult<()> {
		// 1. Count
		let counter_res = self.object_counter_table.count(tx, old, new);
		if let Err(e) = db::unabort(counter_res)? {
			error!(
				"Unable to update object counter: {}. Index values will be wrong!",
				e
			);
		}

		// 2. Enqueue propagation deletions to version table
		if let (Some(old_v), Some(new_v)) = (old, new) {
			for v in old_v.versions.iter() {
				let new_v_id = new_v
					.versions
					.binary_search_by(|nv| nv.cmp_key().cmp(&v.cmp_key()));

				// Propagate deletion of old versions to the Version table
				let delete_version = match new_v_id {
					Err(_) => true,
					Ok(i) => {
						new_v.versions[i].state == ObjectVersionState::Aborted
							&& v.state != ObjectVersionState::Aborted
					}
				};
				if delete_version {
					let deleted_version = Version::new(
						v.uuid,
						VersionBacklink::Object {
							bucket_id: old_v.bucket_id,
							key: old_v.key.clone(),
						},
						true,
					);
					let res = self.version_table.queue_insert(tx, &deleted_version);
					if let Err(e) = db::unabort(res)? {
						error!(
							"Unable to enqueue version deletion propagation: {}. A repair will be needed.",
							e
						);
					}
				}

				// After abortion or completion of multipart uploads, delete MPU table entry
				if matches!(
					v.state,
					ObjectVersionState::Uploading {
						multipart: true,
						..
					}
				) {
					let delete_mpu = match new_v_id {
						Err(_) => true,
						Ok(i) => !matches!(
							new_v.versions[i].state,
							ObjectVersionState::Uploading { .. }
						),
					};
					if delete_mpu {
						let deleted_mpu = MultipartUpload::new(
							v.uuid,
							v.timestamp,
							old_v.bucket_id,
							old_v.key.clone(),
							true,
						);
						let res = self.mpu_table.queue_insert(tx, &deleted_mpu);
						if let Err(e) = db::unabort(res)? {
							error!(
								"Unable to enqueue multipart upload deletion propagation: {}. A repair will be needed.",
								e
							);
						}
					}
				}
			}
		}

		Ok(())
	}

	fn matches_filter(entry: &Self::E, filter: &Self::Filter) -> bool {
		match filter {
			ObjectFilter::IsData => entry.versions.iter().any(|v| v.is_data()),
			ObjectFilter::IsUploading { check_multipart } => entry
				.versions
				.iter()
				.any(|v| v.is_uploading(*check_multipart)),
		}
	}
}

impl CountedItem for Object {
	const COUNTER_TABLE_NAME: &'static str = "bucket_object_counter";

	// Partition key = bucket id
	type CP = Uuid;
	// Sort key = nothing
	type CS = EmptyKey;

	fn counter_partition_key(&self) -> &Uuid {
		&self.bucket_id
	}
	fn counter_sort_key(&self) -> &EmptyKey {
		&EmptyKey
	}

	fn counts(&self) -> Vec<(&'static str, i64)> {
		let versions = self.versions();
		let n_objects = if versions.iter().any(|v| v.is_data()) {
			1
		} else {
			0
		};
		let n_unfinished_uploads = versions.iter().filter(|v| v.is_uploading(None)).count();
		let n_bytes = versions
			.iter()
			.map(|v| match &v.state {
				ObjectVersionState::Complete(ObjectVersionData::Inline(meta, _))
				| ObjectVersionState::Complete(ObjectVersionData::FirstBlock(meta, _)) => meta.size,
				_ => 0,
			})
			.sum::<u64>();

		vec![
			(OBJECTS, n_objects),
			(UNFINISHED_UPLOADS, n_unfinished_uploads as i64),
			(BYTES, n_bytes as i64),
		]
	}
}
