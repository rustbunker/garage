use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{stream, stream::Stream, StreamExt};
use md5::{Digest as Md5Digest, Md5};

use bytes::Bytes;
use hyper::{Body, Request, Response};
use serde::Serialize;

use garage_rpc::netapp::bytes_buf::BytesBuf;
use garage_rpc::rpc_helper::OrderTag;
use garage_table::*;
use garage_util::data::*;
use garage_util::time::*;

use garage_model::garage::Garage;
use garage_model::key_table::Key;
use garage_model::s3::block_ref_table::*;
use garage_model::s3::mpu_table::*;
use garage_model::s3::object_table::*;
use garage_model::s3::version_table::*;

use crate::helpers::parse_bucket_key;
use crate::s3::error::*;
use crate::s3::multipart;
use crate::s3::put::get_headers;
use crate::s3::xml::{self as s3_xml, xmlns_tag};

pub async fn handle_copy(
	garage: Arc<Garage>,
	api_key: &Key,
	req: &Request<Body>,
	dest_bucket_id: Uuid,
	dest_key: &str,
) -> Result<Response<Body>, Error> {
	let copy_precondition = CopyPreconditionHeaders::parse(req)?;

	let source_object = get_copy_source(&garage, api_key, req).await?;

	let (source_version, source_version_data, source_version_meta) =
		extract_source_info(&source_object)?;

	// Check precondition, e.g. x-amz-copy-source-if-match
	copy_precondition.check(source_version, &source_version_meta.etag)?;

	// Generate parameters for copied object
	let new_uuid = gen_uuid();
	let new_timestamp = now_msec();

	// Implement x-amz-metadata-directive: REPLACE
	let new_meta = match req.headers().get("x-amz-metadata-directive") {
		Some(v) if v == hyper::header::HeaderValue::from_static("REPLACE") => ObjectVersionMeta {
			headers: get_headers(req.headers())?,
			size: source_version_meta.size,
			etag: source_version_meta.etag.clone(),
		},
		_ => source_version_meta.clone(),
	};

	let etag = new_meta.etag.to_string();

	// Save object copy
	match source_version_data {
		ObjectVersionData::DeleteMarker => unreachable!(),
		ObjectVersionData::Inline(_meta, bytes) => {
			let dest_object_version = ObjectVersion {
				uuid: new_uuid,
				timestamp: new_timestamp,
				state: ObjectVersionState::Complete(ObjectVersionData::Inline(
					new_meta,
					bytes.clone(),
				)),
			};
			let dest_object = Object::new(
				dest_bucket_id,
				dest_key.to_string(),
				vec![dest_object_version],
			);
			garage.object_table.insert(&dest_object).await?;
		}
		ObjectVersionData::FirstBlock(_meta, first_block_hash) => {
			// Get block list from source version
			let source_version = garage
				.version_table
				.get(&source_version.uuid, &EmptyKey)
				.await?;
			let source_version = source_version.ok_or(Error::NoSuchKey)?;

			// Write an "uploading" marker in Object table
			// This holds a reference to the object in the Version table
			// so that it won't be deleted, e.g. by repair_versions.
			let tmp_dest_object_version = ObjectVersion {
				uuid: new_uuid,
				timestamp: new_timestamp,
				state: ObjectVersionState::Uploading {
					headers: new_meta.headers.clone(),
					multipart: false,
				},
			};
			let tmp_dest_object = Object::new(
				dest_bucket_id,
				dest_key.to_string(),
				vec![tmp_dest_object_version],
			);
			garage.object_table.insert(&tmp_dest_object).await?;

			// Write version in the version table. Even with empty block list,
			// this means that the BlockRef entries linked to this version cannot be
			// marked as deleted (they are marked as deleted only if the Version
			// doesn't exist or is marked as deleted).
			let mut dest_version = Version::new(
				new_uuid,
				VersionBacklink::Object {
					bucket_id: dest_bucket_id,
					key: dest_key.to_string(),
				},
				false,
			);
			garage.version_table.insert(&dest_version).await?;

			// Fill in block list for version and insert block refs
			for (bk, bv) in source_version.blocks.items().iter() {
				dest_version.blocks.put(*bk, *bv);
			}
			let dest_block_refs = dest_version
				.blocks
				.items()
				.iter()
				.map(|b| BlockRef {
					block: b.1.hash,
					version: new_uuid,
					deleted: false.into(),
				})
				.collect::<Vec<_>>();
			futures::try_join!(
				garage.version_table.insert(&dest_version),
				garage.block_ref_table.insert_many(&dest_block_refs[..]),
			)?;

			// Insert final object
			// We do this last because otherwise there is a race condition in the case where
			// the copy call has the same source and destination (this happens, rclone does
			// it to update the modification timestamp for instance). If we did this concurrently
			// with the stuff before, the block's reference counts could be decremented before
			// they are incremented again for the new version, leading to data being deleted.
			let dest_object_version = ObjectVersion {
				uuid: new_uuid,
				timestamp: new_timestamp,
				state: ObjectVersionState::Complete(ObjectVersionData::FirstBlock(
					new_meta,
					*first_block_hash,
				)),
			};
			let dest_object = Object::new(
				dest_bucket_id,
				dest_key.to_string(),
				vec![dest_object_version],
			);
			garage.object_table.insert(&dest_object).await?;
		}
	}

	let last_modified = msec_to_rfc3339(new_timestamp);
	let result = CopyObjectResult {
		last_modified: s3_xml::Value(last_modified),
		etag: s3_xml::Value(format!("\"{}\"", etag)),
	};
	let xml = s3_xml::to_xml_with_header(&result)?;

	Ok(Response::builder()
		.header("Content-Type", "application/xml")
		.header("x-amz-version-id", hex::encode(new_uuid))
		.header(
			"x-amz-copy-source-version-id",
			hex::encode(source_version.uuid),
		)
		.body(Body::from(xml))?)
}

pub async fn handle_upload_part_copy(
	garage: Arc<Garage>,
	api_key: &Key,
	req: &Request<Body>,
	dest_bucket_id: Uuid,
	dest_key: &str,
	part_number: u64,
	upload_id: &str,
) -> Result<Response<Body>, Error> {
	let copy_precondition = CopyPreconditionHeaders::parse(req)?;

	let dest_upload_id = multipart::decode_upload_id(upload_id)?;

	let dest_key = dest_key.to_string();
	let (source_object, (_, _, mut dest_mpu)) = futures::try_join!(
		get_copy_source(&garage, api_key, req),
		multipart::get_upload(&garage, &dest_bucket_id, &dest_key, &dest_upload_id)
	)?;

	let (source_object_version, source_version_data, source_version_meta) =
		extract_source_info(&source_object)?;

	// Check precondition on source, e.g. x-amz-copy-source-if-match
	copy_precondition.check(source_object_version, &source_version_meta.etag)?;

	// Check source range is valid
	let source_range = match req.headers().get("x-amz-copy-source-range") {
		Some(range) => {
			let range_str = range.to_str()?;
			let mut ranges = http_range::HttpRange::parse(range_str, source_version_meta.size)
				.map_err(|e| (e, source_version_meta.size))?;
			if ranges.len() != 1 {
				return Err(Error::bad_request(
					"Invalid x-amz-copy-source-range header: exactly 1 range must be given",
				));
			} else {
				ranges.pop().unwrap()
			}
		}
		None => http_range::HttpRange {
			start: 0,
			length: source_version_meta.size,
		},
	};

	// Check source version is not inlined
	match source_version_data {
		ObjectVersionData::DeleteMarker => unreachable!(),
		ObjectVersionData::Inline(_meta, _bytes) => {
			// This is only for small files, we don't bother handling this.
			// (in AWS UploadPartCopy works for parts at least 5MB which
			// is never the case of an inline object)
			return Err(Error::bad_request(
				"Source object is too small (minimum part size is 5Mb)",
			));
		}
		ObjectVersionData::FirstBlock(_meta, _first_block_hash) => (),
	};

	// Fetch source versin with its block list,
	// and destination version to check part hasn't yet been uploaded
	let source_version = garage
		.version_table
		.get(&source_object_version.uuid, &EmptyKey)
		.await?
		.ok_or(Error::NoSuchKey)?;

	// We want to reuse blocks from the source version as much as possible.
	// However, we still need to get the data from these blocks
	// because we need to know it to calculate the MD5sum of the part
	// which is used as its ETag.

	// First, calculate what blocks we want to keep,
	// and the subrange of the block to take, if the bounds of the
	// requested range are in the middle.
	let (range_begin, range_end) = (source_range.start, source_range.start + source_range.length);

	let mut blocks_to_copy = vec![];
	let mut current_offset = 0;
	for (_bk, block) in source_version.blocks.items().iter() {
		let (block_begin, block_end) = (current_offset, current_offset + block.size);

		if block_begin < range_end && block_end > range_begin {
			let subrange_begin = if block_begin < range_begin {
				Some(range_begin - block_begin)
			} else {
				None
			};
			let subrange_end = if block_end > range_end {
				Some(range_end - block_begin)
			} else {
				None
			};
			let range_to_copy = match (subrange_begin, subrange_end) {
				(Some(b), Some(e)) => Some(b as usize..e as usize),
				(None, Some(e)) => Some(0..e as usize),
				(Some(b), None) => Some(b as usize..block.size as usize),
				(None, None) => None,
			};

			blocks_to_copy.push((block.hash, range_to_copy));
		}

		current_offset = block_end;
	}

	// Calculate the identity of destination part: timestamp, version id
	let dest_version_id = gen_uuid();
	let dest_mpu_part_key = MpuPartKey {
		part_number,
		timestamp: dest_mpu.next_timestamp(part_number),
	};

	// Create the uploaded part
	dest_mpu.parts.clear();
	dest_mpu.parts.put(
		dest_mpu_part_key,
		MpuPart {
			version: dest_version_id,
			etag: None,
			size: None,
		},
	);
	garage.mpu_table.insert(&dest_mpu).await?;

	let mut dest_version = Version::new(
		dest_version_id,
		VersionBacklink::MultipartUpload {
			upload_id: dest_upload_id,
		},
		false,
	);

	// Now, actually copy the blocks
	let mut md5hasher = Md5::new();

	// First, create a stream that is able to read the source blocks
	// and extract the subrange if necessary.
	// The second returned value is an Option<Hash>, that is Some
	// if and only if the block returned is a block that already existed
	// in the Garage data store (thus we don't need to save it again).
	let garage2 = garage.clone();
	let order_stream = OrderTag::stream();
	let source_blocks = stream::iter(blocks_to_copy)
		.enumerate()
		.flat_map(|(i, (block_hash, range_to_copy))| {
			let garage3 = garage2.clone();
			stream::once(async move {
				let data = garage3
					.block_manager
					.rpc_get_block(&block_hash, Some(order_stream.order(i as u64)))
					.await?;
				match range_to_copy {
					Some(r) => Ok((data.slice(r), None)),
					None => Ok((data, Some(block_hash))),
				}
			})
		})
		.peekable();

	// The defragmenter is a custom stream (defined below) that concatenates
	// consecutive block parts when they are too small.
	// It returns a series of (Vec<u8>, Option<Hash>).
	// When it is done, it returns an empty vec.
	// Same as the previous iterator, the Option is Some(_) if and only if
	// it's an existing block of the Garage data store.
	let mut defragmenter = Defragmenter::new(garage.config.block_size, Box::pin(source_blocks));

	let mut current_offset = 0;
	let mut next_block = defragmenter.next().await?;

	loop {
		let (data, existing_block_hash) = next_block;
		if data.is_empty() {
			break;
		}

		md5hasher.update(&data[..]);

		let must_upload = existing_block_hash.is_none();
		let final_hash = existing_block_hash.unwrap_or_else(|| blake2sum(&data[..]));

		dest_version.blocks.clear();
		dest_version.blocks.put(
			VersionBlockKey {
				part_number,
				offset: current_offset,
			},
			VersionBlock {
				hash: final_hash,
				size: data.len() as u64,
			},
		);
		current_offset += data.len() as u64;

		let block_ref = BlockRef {
			block: final_hash,
			version: dest_version_id,
			deleted: false.into(),
		};

		let garage2 = garage.clone();
		let res = futures::try_join!(
			// Thing 1: if the block is not exactly a block that existed before,
			// we need to insert that data as a new block.
			async move {
				if must_upload {
					garage2.block_manager.rpc_put_block(final_hash, data).await
				} else {
					Ok(())
				}
			},
			async {
				// Thing 2: we need to insert the block in the version
				garage.version_table.insert(&dest_version).await?;
				// Thing 3: we need to add a block reference
				garage.block_ref_table.insert(&block_ref).await
			},
			// Thing 4: we need to prefetch the next block
			defragmenter.next(),
		)?;
		next_block = res.2;
	}

	assert_eq!(current_offset, source_range.length);

	let data_md5sum = md5hasher.finalize();
	let etag = hex::encode(data_md5sum);

	// Put the part's ETag in the Versiontable
	dest_mpu.parts.put(
		dest_mpu_part_key,
		MpuPart {
			version: dest_version_id,
			etag: Some(etag.clone()),
			size: Some(current_offset),
		},
	);
	garage.mpu_table.insert(&dest_mpu).await?;

	// LGTM
	let resp_xml = s3_xml::to_xml_with_header(&CopyPartResult {
		xmlns: (),
		etag: s3_xml::Value(format!("\"{}\"", etag)),
		last_modified: s3_xml::Value(msec_to_rfc3339(source_object_version.timestamp)),
	})?;

	Ok(Response::builder()
		.header("Content-Type", "application/xml")
		.header(
			"x-amz-copy-source-version-id",
			hex::encode(source_object_version.uuid),
		)
		.body(Body::from(resp_xml))?)
}

async fn get_copy_source(
	garage: &Garage,
	api_key: &Key,
	req: &Request<Body>,
) -> Result<Object, Error> {
	let copy_source = req.headers().get("x-amz-copy-source").unwrap().to_str()?;
	let copy_source = percent_encoding::percent_decode_str(copy_source).decode_utf8()?;

	let (source_bucket, source_key) = parse_bucket_key(&copy_source, None)?;
	let source_bucket_id = garage
		.bucket_helper()
		.resolve_bucket(&source_bucket.to_string(), api_key)
		.await?;

	if !api_key.allow_read(&source_bucket_id) {
		return Err(Error::forbidden(format!(
			"Reading from bucket {} not allowed for this key",
			source_bucket
		)));
	}

	let source_key = source_key.ok_or_bad_request("No source key specified")?;

	let source_object = garage
		.object_table
		.get(&source_bucket_id, &source_key.to_string())
		.await?
		.ok_or(Error::NoSuchKey)?;

	Ok(source_object)
}

fn extract_source_info(
	source_object: &Object,
) -> Result<(&ObjectVersion, &ObjectVersionData, &ObjectVersionMeta), Error> {
	let source_version = source_object
		.versions()
		.iter()
		.rev()
		.find(|v| v.is_complete())
		.ok_or(Error::NoSuchKey)?;

	let source_version_data = match &source_version.state {
		ObjectVersionState::Complete(x) => x,
		_ => unreachable!(),
	};

	let source_version_meta = match source_version_data {
		ObjectVersionData::DeleteMarker => {
			return Err(Error::NoSuchKey);
		}
		ObjectVersionData::Inline(meta, _bytes) => meta,
		ObjectVersionData::FirstBlock(meta, _fbh) => meta,
	};

	Ok((source_version, source_version_data, source_version_meta))
}

struct CopyPreconditionHeaders {
	copy_source_if_match: Option<Vec<String>>,
	copy_source_if_modified_since: Option<SystemTime>,
	copy_source_if_none_match: Option<Vec<String>>,
	copy_source_if_unmodified_since: Option<SystemTime>,
}

impl CopyPreconditionHeaders {
	fn parse(req: &Request<Body>) -> Result<Self, Error> {
		Ok(Self {
			copy_source_if_match: req
				.headers()
				.get("x-amz-copy-source-if-match")
				.map(|x| x.to_str())
				.transpose()?
				.map(|x| {
					x.split(',')
						.map(|m| m.trim().trim_matches('"').to_string())
						.collect::<Vec<_>>()
				}),
			copy_source_if_modified_since: req
				.headers()
				.get("x-amz-copy-source-if-modified-since")
				.map(|x| x.to_str())
				.transpose()?
				.map(httpdate::parse_http_date)
				.transpose()
				.ok_or_bad_request("Invalid date in x-amz-copy-source-if-modified-since")?,
			copy_source_if_none_match: req
				.headers()
				.get("x-amz-copy-source-if-none-match")
				.map(|x| x.to_str())
				.transpose()?
				.map(|x| {
					x.split(',')
						.map(|m| m.trim().trim_matches('"').to_string())
						.collect::<Vec<_>>()
				}),
			copy_source_if_unmodified_since: req
				.headers()
				.get("x-amz-copy-source-if-unmodified-since")
				.map(|x| x.to_str())
				.transpose()?
				.map(httpdate::parse_http_date)
				.transpose()
				.ok_or_bad_request("Invalid date in x-amz-copy-source-if-unmodified-since")?,
		})
	}

	fn check(&self, v: &ObjectVersion, etag: &str) -> Result<(), Error> {
		let v_date = UNIX_EPOCH + Duration::from_millis(v.timestamp);

		let ok = match (
			&self.copy_source_if_match,
			&self.copy_source_if_unmodified_since,
			&self.copy_source_if_none_match,
			&self.copy_source_if_modified_since,
		) {
			// TODO I'm not sure all of the conditions are evaluated correctly here

			// If we have both if-match and if-unmodified-since,
			// basically we don't care about if-unmodified-since,
			// because in the spec it says that if if-match evaluates to
			// true but if-unmodified-since evaluates to false,
			// the copy is still done.
			(Some(im), _, None, None) => im.iter().any(|x| x == etag || x == "*"),
			(None, Some(ius), None, None) => v_date <= *ius,

			// If we have both if-none-match and if-modified-since,
			// then both of the two conditions must evaluate to true
			(None, None, Some(inm), Some(ims)) => {
				!inm.iter().any(|x| x == etag || x == "*") && v_date > *ims
			}
			(None, None, Some(inm), None) => !inm.iter().any(|x| x == etag || x == "*"),
			(None, None, None, Some(ims)) => v_date > *ims,
			(None, None, None, None) => true,
			_ => {
				return Err(Error::bad_request(
					"Invalid combination of x-amz-copy-source-if-xxxxx headers",
				))
			}
		};

		if ok {
			Ok(())
		} else {
			Err(Error::PreconditionFailed)
		}
	}
}

type BlockStreamItemOk = (Bytes, Option<Hash>);
type BlockStreamItem = Result<BlockStreamItemOk, garage_util::error::Error>;

struct Defragmenter<S: Stream<Item = BlockStreamItem>> {
	block_size: usize,
	block_stream: Pin<Box<stream::Peekable<S>>>,
	buffer: BytesBuf,
	hash: Option<Hash>,
}

impl<S: Stream<Item = BlockStreamItem>> Defragmenter<S> {
	fn new(block_size: usize, block_stream: Pin<Box<stream::Peekable<S>>>) -> Self {
		Self {
			block_size,
			block_stream,
			buffer: BytesBuf::new(),
			hash: None,
		}
	}

	async fn next(&mut self) -> BlockStreamItem {
		// Fill buffer while we can
		while let Some(res) = self.block_stream.as_mut().peek().await {
			let (peeked_next_block, _) = match res {
				Ok(t) => t,
				Err(_) => {
					self.block_stream.next().await.unwrap()?;
					unreachable!()
				}
			};

			if self.buffer.is_empty() {
				let (next_block, next_block_hash) = self.block_stream.next().await.unwrap()?;
				self.buffer.extend(next_block);
				self.hash = next_block_hash;
			} else if self.buffer.len() + peeked_next_block.len() > self.block_size {
				break;
			} else {
				let (next_block, _) = self.block_stream.next().await.unwrap()?;
				self.buffer.extend(next_block);
				self.hash = None;
			}
		}

		Ok((self.buffer.take_all(), self.hash.take()))
	}
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CopyObjectResult {
	#[serde(rename = "LastModified")]
	pub last_modified: s3_xml::Value,
	#[serde(rename = "ETag")]
	pub etag: s3_xml::Value,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CopyPartResult {
	#[serde(serialize_with = "xmlns_tag")]
	pub xmlns: (),
	#[serde(rename = "LastModified")]
	pub last_modified: s3_xml::Value,
	#[serde(rename = "ETag")]
	pub etag: s3_xml::Value,
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::s3::xml::to_xml_with_header;

	#[test]
	fn copy_object_result() -> Result<(), Error> {
		let copy_result = CopyObjectResult {
			last_modified: s3_xml::Value(msec_to_rfc3339(0)),
			etag: s3_xml::Value("\"9b2cf535f27731c974343645a3985328\"".to_string()),
		};
		assert_eq!(
			to_xml_with_header(&copy_result)?,
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<CopyObjectResult>\
    <LastModified>1970-01-01T00:00:00.000Z</LastModified>\
    <ETag>&quot;9b2cf535f27731c974343645a3985328&quot;</ETag>\
</CopyObjectResult>\
			"
		);
		Ok(())
	}

	#[test]
	fn serialize_copy_part_result() -> Result<(), Error> {
		let expected_retval = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<CopyPartResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
	<LastModified>2011-04-11T20:34:56.000Z</LastModified>\
	<ETag>&quot;9b2cf535f27731c974343645a3985328&quot;</ETag>\
</CopyPartResult>";
		let v = CopyPartResult {
			xmlns: (),
			last_modified: s3_xml::Value("2011-04-11T20:34:56.000Z".into()),
			etag: s3_xml::Value("\"9b2cf535f27731c974343645a3985328\"".into()),
		};

		assert_eq!(to_xml_with_header(&v)?, expected_retval);

		Ok(())
	}
}
