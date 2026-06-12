//! Tee-import support for the proxy-on-miss path.
//!
//! When a cold upload-pack request is proxied to upstream, the response pack
//! is byte-for-byte the pack the cache itself would fetch for the same wants.
//! Instead of re-downloading it in the background warm, the API layer demuxes
//! the proxied response with [`PackDemux`], spools the pack bytes to disk, and
//! imports them via [`Materializer::import_proxied_upload_pack`]. Any
//! failure along the way falls back to the existing background warm refetch.

use super::direct_git::{
    parse_upload_pack_intent, UploadPackFilter, UploadPackIntent, PARTIAL_HYDRATION_MARKER,
};
use super::*;

/// Upper bound on bytes buffered while reassembling a single pkt-line frame
/// (max pkt payload is 65516 bytes; leave headroom).
const DEMUX_MAX_BUFFER_BYTES: usize = 128 * 1024;

/// Eligibility plan for tee-importing a proxied upload-pack response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPackTeePlan {
    /// Whether the client negotiated side-band/side-band-64k, i.e. the
    /// response pack arrives multiplexed in band-1 pkt frames.
    pub sideband: bool,
    /// Whether the request carries a `filter blob:none` line.
    pub blobless: bool,
}

/// Decide whether a proxied upload-pack response can be tee-imported.
///
/// Only full-closure responses are safe to import: the request must carry at
/// least one want, no shallow/deepen lines, and no `have` lines (a client
/// with haves receives a thin, incremental pack that is not self-contained).
/// Protocol v2 bodies (`command=` lines) are declined; the cache advertises
/// v0 so direct clients negotiate v0.
pub fn plan_upload_pack_tee(body: &[u8]) -> Option<UploadPackTeePlan> {
    let intent = parse_upload_pack_intent(body).ok()?;
    if intent.wants.is_empty()
        || intent.depth.is_some()
        || intent.deepen_since.is_some()
        || !intent.deepen_not.is_empty()
        || !intent.shallow.is_empty()
    {
        return None;
    }

    let mut has_haves = false;
    let mut is_v2 = false;
    let mut sideband = false;
    super::direct_git::visit_upload_pack_lines(body, |line| {
        let line = line.trim_end();
        if line.starts_with("have ") {
            has_haves = true;
        } else if line.starts_with("command=") {
            is_v2 = true;
        } else if let Some(rest) = line.strip_prefix("want ") {
            if rest.contains("side-band") {
                sideband = true;
            }
        }
    });
    if has_haves || is_v2 {
        return None;
    }

    Some(UploadPackTeePlan {
        sideband,
        blobless: intent.filter == Some(UploadPackFilter::BlobNone),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DemuxState {
    /// Parsing pre-pack pkt lines (waiting for the server's `NAK`).
    AwaitNak,
    /// Sideband: parsing multiplexed pkt frames, extracting band 1.
    Sideband,
    /// No sideband: all remaining raw bytes are pack data.
    RawPack,
    /// Sideband terminal flush seen; ignore any trailing bytes.
    Done,
    Failed,
}

/// Incremental demuxer for a protocol v0 stateless-rpc upload-pack response.
///
/// The response is `NAK` (plus possible ACK lines) followed by either raw
/// pack bytes (no side-band) or band-1 pkt frames terminated by a flush
/// (side-band). Progress (band 2) is discarded; band 3 or `ERR` fails the
/// demux. The extracted pack bytes are appended to the caller's sink.
#[derive(Debug)]
pub struct PackDemux {
    state: DemuxState,
    sideband: bool,
    buf: Vec<u8>,
    pack_bytes: u64,
}

impl PackDemux {
    pub fn new(sideband: bool) -> Self {
        Self {
            state: DemuxState::AwaitNak,
            sideband,
            buf: Vec::new(),
            pack_bytes: 0,
        }
    }

    pub fn pack_bytes(&self) -> u64 {
        self.pack_bytes
    }

    /// Whether the demux ended in a state where the spooled bytes form a
    /// complete pack candidate: pack data was seen and nothing failed.
    /// `git index-pack` remains the authoritative integrity check.
    pub fn pack_complete(&self) -> bool {
        self.pack_bytes > 0
            && matches!(
                self.state,
                DemuxState::Done | DemuxState::RawPack | DemuxState::Sideband
            )
    }

    /// Feed a response chunk, appending extracted pack bytes to `sink`.
    pub fn feed(&mut self, chunk: &[u8], sink: &mut Vec<u8>) -> CoreResult<()> {
        if self.state == DemuxState::Failed {
            return Err(GitCacheError::Validation(
                "upload-pack response demux already failed".into(),
            ));
        }
        if self.state == DemuxState::RawPack {
            self.pack_bytes += chunk.len() as u64;
            sink.extend_from_slice(chunk);
            return Ok(());
        }
        if self.state == DemuxState::Done {
            return Ok(());
        }

        self.buf.extend_from_slice(chunk);
        if self.buf.len() > DEMUX_MAX_BUFFER_BYTES {
            return self.fail("pkt-line frame exceeded buffer limit");
        }

        let mut offset = 0;
        loop {
            let remaining = &self.buf[offset..];
            if remaining.len() < 4 {
                break;
            }
            let Some(pkt_len) = parse_pkt_len(&remaining[..4]) else {
                return self.fail("invalid pkt-line length in upload-pack response");
            };
            if pkt_len == 0 {
                // Flush pkt.
                offset += 4;
                match self.state {
                    DemuxState::Sideband => {
                        self.state = DemuxState::Done;
                        self.buf.clear();
                        return Ok(());
                    }
                    _ => continue,
                }
            }
            if pkt_len < 5 {
                return self.fail("invalid short pkt-line in upload-pack response");
            }
            if remaining.len() < pkt_len {
                break;
            }
            let payload = &remaining[4..pkt_len];
            match self.state {
                DemuxState::AwaitNak => {
                    if payload.starts_with(b"NAK") {
                        if self.sideband {
                            self.state = DemuxState::Sideband;
                        } else {
                            // Remaining bytes after this pkt are raw pack data.
                            let rest_start = offset + pkt_len;
                            let rest = self.buf[rest_start..].to_vec();
                            self.pack_bytes += rest.len() as u64;
                            sink.extend_from_slice(&rest);
                            self.buf.clear();
                            self.state = DemuxState::RawPack;
                            return Ok(());
                        }
                    } else if payload.starts_with(b"ACK")
                        || payload.starts_with(b"shallow")
                        || payload.starts_with(b"unshallow")
                    {
                        // Tolerated pre-pack lines; eligibility should have
                        // excluded requests that produce shallow lines.
                    } else {
                        return self.fail("unexpected pre-pack line in upload-pack response");
                    }
                }
                DemuxState::Sideband => match payload[0] {
                    1 => {
                        self.pack_bytes += (payload.len() - 1) as u64;
                        sink.extend_from_slice(&payload[1..]);
                    }
                    2 => {}
                    _ => {
                        return self.fail("upload-pack response reported a sideband error");
                    }
                },
                DemuxState::RawPack | DemuxState::Done | DemuxState::Failed => unreachable!(),
            }
            offset += pkt_len;
        }
        self.buf.drain(..offset);
        Ok(())
    }

    fn fail(&mut self, message: &str) -> CoreResult<()> {
        self.state = DemuxState::Failed;
        self.buf.clear();
        Err(GitCacheError::Validation(message.into()))
    }
}

fn parse_pkt_len(header: &[u8]) -> Option<usize> {
    let hex = std::str::from_utf8(header).ok()?;
    usize::from_str_radix(hex, 16).ok()
}

impl Materializer {
    /// Import a spooled proxied upload-pack response pack into the shared
    /// bare repo, making the proxied wants servable from cache without a
    /// second upstream download.
    ///
    /// `spool_path` is moved into `objects/pack/pack-<sha256>.pack` and
    /// indexed; blobless imports get a pack-level `.promisor` marker plus the
    /// repo-level partial-hydration marker so later full-object requests
    /// force a refetch. On any failure the placed pack files are removed and
    /// the error propagates so the caller can fall back to the warm refetch.
    pub async fn import_proxied_upload_pack(
        &self,
        repo: &RepoKey,
        body: &Bytes,
        spool_path: &FsPath,
        pack_sha256: &str,
    ) -> CoreResult<()> {
        let intent = parse_upload_pack_intent(body)?;
        if intent.wants.is_empty() {
            return Err(GitCacheError::Validation(
                "tee import requires at least one want".into(),
            ));
        }
        if pack_sha256.len() != 64
            || !pack_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(GitCacheError::Validation(
                "tee import pack sha256 must be 64 lowercase hex chars".into(),
            ));
        }
        let blobless = intent.filter == Some(UploadPackFilter::BlobNone);

        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;

        // A full-object pack cannot safely clear an existing blobless
        // marker (other refs may still be partial); decline so the warm
        // refetch path performs its authoritative unfiltered `--refetch`.
        let partial_marker = repo_dir.join(PARTIAL_HYDRATION_MARKER);
        if !blobless && fs::try_exists(&partial_marker).await? {
            return Err(GitCacheError::Conflict(
                "repo is partially hydrated; tee import deferred to warm refetch".into(),
            ));
        }

        self.configure_served_repo(&repo_dir).await?;
        let pack_dir = repo_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).await?;
        let final_path = pack_dir.join(format!("pack-{pack_sha256}.pack"));
        if fs::rename(spool_path, &final_path).await.is_err() {
            fs::copy(spool_path, &final_path).await?;
            let _ = fs::remove_file(spool_path).await;
        }

        let result = self
            .finish_tee_import(repo, &repo_dir, &intent, &final_path, blobless)
            .await;
        if result.is_err() {
            for ext in ["pack", "idx", "rev", "promisor", "keep"] {
                let _ = fs::remove_file(final_path.with_extension(ext)).await;
            }
        }
        result
    }

    async fn finish_tee_import(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        intent: &UploadPackIntent,
        pack_path: &FsPath,
        blobless: bool,
    ) -> CoreResult<()> {
        let index_started = Instant::now();
        self.state.git.index_pack(repo_dir, pack_path).await?;
        if blobless {
            // Mark the pack as a promisor pack so connectivity checks
            // (fsck, index-pack of later packs) tolerate the missing blobs,
            // mirroring what a real `fetch --filter=blob:none` records.
            fs::write(pack_path.with_extension("promisor"), b"tee import\n").await?;
            fs::write(repo_dir.join(PARTIAL_HYDRATION_MARKER), b"blobless\n").await?;
        }
        info!(
            %repo,
            blobless,
            elapsed_ms = elapsed_ms(index_started),
            "tee import pack indexed"
        );

        for object_id in &intent.wants {
            self.prepare_fetched_direct_want(repo_dir, object_id)
                .await?;
        }

        if let Some(commit) = intent.wants.first() {
            self.enqueue_direct_fsck(repo.clone(), repo_dir.to_path_buf(), commit.clone());
        }
        self.enqueue_serving_maintenance(repo.clone(), repo_dir.to_path_buf());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
