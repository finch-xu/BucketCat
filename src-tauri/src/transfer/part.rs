//! Upload/download chunking strategy (design §5, §4.2). Pure arithmetic, no IO.
//!
//! Upload and download used to share one planner (`plan_upload`, reused
//! verbatim by the download runner because the offset/length arithmetic is
//! identical). [`TransferTuning`] splits them apart: the upload planner still
//! has to respect S3's hard multipart invariants (non-final parts equal
//! size, ≥5MB, ≤5GB, ≤10 000 parts total -- see [`MIN_PART_SIZE`],
//! [`MAX_PART_SIZE`], [`MAX_PARTS`]), while the download planner answers a
//! different question -- how many concurrent Range GETs to fan out -- with
//! its own threshold, floor and target part count, capped only by
//! [`DOWNLOAD_CHUNK_CAP`]. Both planners bottom out in the same
//! [`chunks_for`] splitter.

/// Lower bound on a part. S3 and OSS both reject non-final parts under 5MB;
/// 8MB keeps a margin and matches design §5. Every preset's
/// `upload_part_floor` stays at or above this.
pub const MIN_PART_SIZE: u64 = 8 * 1024 * 1024;

/// Upper bound on a part, imposed by S3's `UploadPart` API.
pub const MAX_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// The protocol's hard ceiling, asserted against in tests.
pub const MAX_PARTS: u64 = 10_000;

/// Upper bound on a download chunk. A download chunk is not an S3 multipart
/// part -- there is no protocol ceiling to respect -- but an unbounded chunk
/// on a multi-TB object would mean a single Range GET holding a connection
/// open for hours with nothing smaller to retry. 256MB keeps a retry cheap
/// no matter how large the object.
pub const DOWNLOAD_CHUNK_CAP: u64 = 256 * 1024 * 1024;

const MB: u64 = 1024 * 1024;

/// One part of a multipart upload, or one Range GET chunk of a download.
/// `number` is 1-based, matching what S3's multipart API requires (a
/// download reuses the same numbering purely so resume bookkeeping -- which
/// tracks "finished chunk numbers" for both directions -- can stay uniform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartSpec {
    pub number: i32,
    pub offset: u64,
    pub length: u64,
}

/// How a given file will be uploaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadPlan {
    Single {
        length: u64,
    },
    Multipart {
        part_size: u64,
        parts: Vec<PartSpec>,
    },
}

/// How a given object will be downloaded. `chunk_size` is the target size
/// used to derive `chunks` (the last chunk may be shorter); kept alongside
/// the chunks themselves so a caller (the download runner's resume path,
/// once it records `part_size` per task) can persist it without
/// recomputing it from `chunks[0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPlan {
    pub chunk_size: u64,
    pub chunks: Vec<PartSpec>,
}

/// The tuning knobs behind the three presets a user picks from in Settings
/// (M6). Upload and download are tuned independently: an upload's floor and
/// target part count are chosen with S3's multipart protocol in mind, while
/// a download's are a pure concurrency/throughput trade-off bounded only by
/// [`DOWNLOAD_CHUNK_CAP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferTuning {
    /// Files smaller than this go up as a single `PutObject`; multipart's
    /// extra round trips (create / complete, plus per-part overhead) cost
    /// more than they save below it.
    pub upload_threshold: u64,
    /// Lower bound on a computed upload part size.
    pub upload_part_floor: u64,
    /// The upload part count planning aims for; `total.div_ceil(this)` is
    /// the starting point before the floor/[`MAX_PART_SIZE`] clamp.
    pub upload_target_parts: u64,
    /// Objects smaller than this download as a single Range GET.
    pub download_threshold: u64,
    /// Lower bound on a computed download chunk size.
    pub download_chunk_floor: u64,
    /// The download chunk count planning aims for; `total.div_ceil(this)` is
    /// the starting point before the floor/[`DOWNLOAD_CHUNK_CAP`] clamp.
    pub download_target_parts: u64,
}

impl TransferTuning {
    /// Fewer, larger parts/chunks: gentler on flaky or metered connections
    /// at the cost of parallelism.
    pub const fn conservative() -> Self {
        Self {
            upload_threshold: 64 * MB,
            upload_part_floor: 32 * MB,
            upload_target_parts: 16,
            download_threshold: 128 * MB,
            download_chunk_floor: 64 * MB,
            download_target_parts: 8,
        }
    }

    /// The default preset: a middle ground suited to most connections.
    pub const fn balanced() -> Self {
        Self {
            upload_threshold: 32 * MB,
            upload_part_floor: 16 * MB,
            upload_target_parts: 32,
            download_threshold: 64 * MB,
            download_chunk_floor: 32 * MB,
            download_target_parts: 16,
        }
    }

    /// More, smaller parts/chunks: maximizes parallelism on fast, stable
    /// connections at the cost of per-request overhead.
    pub const fn aggressive() -> Self {
        Self {
            upload_threshold: 16 * MB,
            upload_part_floor: 8 * MB,
            upload_target_parts: 100,
            download_threshold: 16 * MB,
            download_chunk_floor: 8 * MB,
            download_target_parts: 64,
        }
    }
}

impl Default for TransferTuning {
    /// Balanced is the default preset (design §4.2).
    fn default() -> Self {
        Self::balanced()
    }
}

/// Splits `total` bytes into equal-`part_size` chunks, the last one short.
///
/// Shared by both planners below, and reused directly by a resume path as
/// `chunks_for(total, recorded_part_size)` to reproduce a prior run's exact
/// chunk table from nothing but those two scalars -- the basis of checkpoint
/// resume, since neither `total` nor a recorded `part_size` drift between
/// runs of the same transfer.
///
/// `total == 0`, `part_size == 0` or `total <= part_size` all collapse to a
/// single chunk spanning the whole object -- 0 bytes included, since a
/// 0-byte object is a real, common case (an M3 folder marker) and a plan
/// with zero parts is not.
pub fn chunks_for(total: u64, part_size: u64) -> Vec<PartSpec> {
    if total == 0 || part_size == 0 || total <= part_size {
        return vec![PartSpec {
            number: 1,
            offset: 0,
            length: total,
        }];
    }
    let mut parts = Vec::with_capacity(total.div_ceil(part_size) as usize);
    let (mut offset, mut number) = (0u64, 1i32);
    while offset < total {
        let length = part_size.min(total - offset);
        parts.push(PartSpec {
            number,
            offset,
            length,
        });
        offset += length;
        number += 1;
    }
    parts
}

/// Splits `total` bytes into an upload plan under tuning `t`.
///
/// Below `t.upload_threshold`, a single `PutObject`. At or above it, a
/// multipart plan whose part size targets `t.upload_target_parts` equal
/// parts, floored at `t.upload_part_floor` and ceilinged at
/// [`MAX_PART_SIZE`] -- the ceiling only binds on multi-TB objects, where it
/// keeps the part count within [`MAX_PARTS`] instead of the server rejecting
/// an oversized part with `EntityTooLarge`.
pub fn plan_upload_with(total: u64, t: &TransferTuning) -> UploadPlan {
    if total < t.upload_threshold {
        return UploadPlan::Single { length: total };
    }
    let part_size = total
        .div_ceil(t.upload_target_parts)
        .clamp(t.upload_part_floor, MAX_PART_SIZE);
    UploadPlan::Multipart {
        part_size,
        parts: chunks_for(total, part_size),
    }
}

/// Splits `total` bytes into a download plan under tuning `t`.
///
/// Below `t.download_threshold`, a single chunk spanning the whole object
/// (a single Range GET, or for a 0-byte object a single length-0 chunk the
/// download runner still fetches, per [`chunks_for`]). At or above it,
/// `t.download_target_parts` equal chunks, floored at
/// `t.download_chunk_floor` and ceilinged at [`DOWNLOAD_CHUNK_CAP`].
pub fn plan_download(total: u64, t: &TransferTuning) -> DownloadPlan {
    if total < t.download_threshold {
        return DownloadPlan {
            chunk_size: total,
            chunks: chunks_for(total, total),
        };
    }
    let chunk_size = total
        .div_ceil(t.download_target_parts)
        .clamp(t.download_chunk_floor, DOWNLOAD_CHUNK_CAP);
    DownloadPlan {
        chunk_size,
        chunks: chunks_for(total, chunk_size),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    #[test]
    fn empty_upload_is_a_single_zero_length_put() {
        // A zero-byte object is legal and common (it is exactly how M3
        // creates folder markers); multipart with zero parts is not. Kept
        // from the pre-split suite: `plan_upload_with` replaces `plan_upload`
        // but must preserve this invariant for every tuning.
        assert_eq!(
            plan_upload_with(0, &TransferTuning::default()),
            UploadPlan::Single { length: 0 }
        );
    }

    #[test]
    fn presets_match_the_spec_table() {
        let b = TransferTuning::balanced();
        assert_eq!(b.upload_threshold, 32 * MB);
        assert_eq!(b.upload_part_floor, 16 * MB);
        assert_eq!(b.upload_target_parts, 32);
        assert_eq!(b.download_threshold, 64 * MB);
        assert_eq!(b.download_chunk_floor, 32 * MB);
        assert_eq!(b.download_target_parts, 16);
        let c = TransferTuning::conservative();
        assert_eq!(
            (
                c.upload_threshold,
                c.upload_part_floor,
                c.upload_target_parts
            ),
            (64 * MB, 32 * MB, 16)
        );
        assert_eq!(
            (
                c.download_threshold,
                c.download_chunk_floor,
                c.download_target_parts
            ),
            (128 * MB, 64 * MB, 8)
        );
        let a = TransferTuning::aggressive();
        assert_eq!(
            (
                a.upload_threshold,
                a.upload_part_floor,
                a.upload_target_parts
            ),
            (16 * MB, 8 * MB, 100)
        );
        assert_eq!(
            (
                a.download_threshold,
                a.download_chunk_floor,
                a.download_target_parts
            ),
            (16 * MB, 8 * MB, 64)
        );
    }

    #[test]
    fn download_request_counts_match_the_spec_examples() {
        // spec §4.2 效果示例表:250MB → 保守 4 / 均衡 8;1GB → 保守 8 / 均衡 16;
        // 100MB → 保守单流、均衡 4。
        assert_eq!(
            plan_download(250 * MB, &TransferTuning::conservative())
                .chunks
                .len(),
            4
        );
        assert_eq!(
            plan_download(250 * MB, &TransferTuning::balanced())
                .chunks
                .len(),
            8
        );
        assert_eq!(
            plan_download(1024 * MB, &TransferTuning::conservative())
                .chunks
                .len(),
            8
        );
        assert_eq!(
            plan_download(1024 * MB, &TransferTuning::balanced())
                .chunks
                .len(),
            16
        );
        assert_eq!(
            plan_download(100 * MB, &TransferTuning::conservative())
                .chunks
                .len(),
            1
        ); // 单流
        assert_eq!(
            plan_download(100 * MB, &TransferTuning::balanced())
                .chunks
                .len(),
            4
        );
    }

    #[test]
    fn download_below_threshold_is_one_chunk() {
        let t = TransferTuning::balanced();
        let p = plan_download(t.download_threshold - 1, &t);
        assert_eq!(p.chunks.len(), 1);
        assert_eq!(
            p.chunks[0],
            PartSpec {
                number: 1,
                offset: 0,
                length: t.download_threshold - 1
            }
        );
        // 0 字节对象:单个 length-0 chunk(下载 runner 依赖这一点)
        assert_eq!(plan_download(0, &t).chunks.len(), 1);
    }

    #[test]
    fn download_chunk_size_is_capped() {
        // 10TB / 8(保守)= 1.25TB,必须被 256MB cap 压住
        let p = plan_download(10 * TB, &TransferTuning::conservative());
        assert_eq!(p.chunk_size, DOWNLOAD_CHUNK_CAP);
    }

    #[test]
    fn upload_invariants_hold_for_every_preset() {
        // 全预设 × 关键尺寸:非末片等大且 ≥5MB、片数 ≤10000、长度求和恒等
        for t in [
            TransferTuning::conservative(),
            TransferTuning::balanced(),
            TransferTuning::aggressive(),
        ] {
            for total in [t.upload_threshold, 100 * MB, 8 * GB, 5 * TB] {
                let UploadPlan::Multipart { part_size, parts } = plan_upload_with(total, &t) else {
                    panic!("{total} should be multipart");
                };
                assert!(parts.len() as u64 <= MAX_PARTS);
                assert!(part_size >= MIN_PART_SIZE);
                let sum: u64 = parts.iter().map(|p| p.length).sum();
                assert_eq!(sum, total);
                for p in &parts[..parts.len() - 1] {
                    assert_eq!(p.length, part_size, "non-final parts must be equal (R2)");
                }
            }
        }
    }

    #[test]
    fn chunks_for_reproduces_a_plan_from_its_part_size() {
        // resume 靠 (total, part_size) 完整复原分片表 —— checkpoint 防错位的基石
        let t = TransferTuning::balanced();
        let plan = plan_download(1024 * MB, &t);
        assert_eq!(chunks_for(1024 * MB, plan.chunk_size), plan.chunks);
    }
}
