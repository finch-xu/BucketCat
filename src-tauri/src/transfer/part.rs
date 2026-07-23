//! Upload chunking strategy (design §5). Pure arithmetic, no IO.

/// Files smaller than this go up as a single `PutObject`; multipart's three
/// extra round trips (create / complete, plus per-part overhead) cost more
/// than they save below it.
pub const MULTIPART_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Lower bound on a part. S3 and OSS both reject non-final parts under 5MB;
/// 8MB keeps a margin and matches design §5.
pub const MIN_PART_SIZE: u64 = 8 * 1024 * 1024;

/// Upper bound on a part, imposed by S3's `UploadPart` API.
pub const MAX_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// Target part count for large files. S3/OSS cap an upload at 10 000 parts;
/// aiming at 1 000 leaves an order of magnitude of headroom, so a future
/// change to the floor can never walk into the hard limit.
pub const PART_DIVISOR: u64 = 1_000;

/// The protocol's hard ceiling, asserted against in tests.
pub const MAX_PARTS: u64 = 10_000;

/// One part of a multipart upload. `number` is 1-based, as S3 requires.
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

/// `max(8MB, ceil(total / 1000))`, clamped to S3's 5GB per-part ceiling.
///
/// The clamp only ever binds above 5TB-scale objects (5TB/1000 = 5.12GB), i.e.
/// right at S3's own max object size -- but without it the plan for such a
/// file would be rejected by the server rather than by us.
#[allow(clippy::manual_clamp)]
pub fn part_size_for(total: u64) -> u64 {
    total
        .div_ceil(PART_DIVISOR)
        .max(MIN_PART_SIZE)
        .min(MAX_PART_SIZE)
}

/// Splits `total` bytes into an upload plan.
pub fn plan_upload(total: u64) -> UploadPlan {
    if total < MULTIPART_THRESHOLD {
        return UploadPlan::Single { length: total };
    }

    let part_size = part_size_for(total);
    let count = total.div_ceil(part_size);
    let mut parts = Vec::with_capacity(count as usize);

    let mut offset = 0u64;
    let mut number = 1i32;
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

    UploadPlan::Multipart { part_size, parts }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    fn parts_of(plan: &UploadPlan) -> &[PartSpec] {
        match plan {
            UploadPlan::Multipart { parts, .. } => parts,
            UploadPlan::Single { .. } => panic!("expected a multipart plan"),
        }
    }

    #[test]
    fn empty_file_is_a_single_zero_length_put() {
        // A zero-byte object is legal and common (it is exactly how M3
        // creates folder markers); multipart with zero parts is not.
        assert_eq!(plan_upload(0), UploadPlan::Single { length: 0 });
    }

    #[test]
    fn below_the_threshold_stays_single_stream() {
        assert_eq!(plan_upload(1), UploadPlan::Single { length: 1 });
        assert_eq!(
            plan_upload(MULTIPART_THRESHOLD - 1),
            UploadPlan::Single {
                length: MULTIPART_THRESHOLD - 1
            }
        );
    }

    #[test]
    fn exactly_at_the_threshold_goes_multipart() {
        // Design §5 says "< 16MB single stream", so 16MB itself is multipart.
        let plan = plan_upload(MULTIPART_THRESHOLD);
        let parts = parts_of(&plan);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].length, MIN_PART_SIZE);
        assert_eq!(parts[1].length, MIN_PART_SIZE);
    }

    #[test]
    fn hundred_megabytes_uses_the_floor_part_size() {
        // 100MB / 1000 is far below 8MB, so the 8MB floor wins.
        let plan = plan_upload(100 * MB);
        assert_eq!(part_size_for(100 * MB), MIN_PART_SIZE);
        let parts = parts_of(&plan);
        assert_eq!(parts.len(), 13);
        assert_eq!(parts[0].number, 1);
        assert_eq!(parts[0].offset, 0);
        assert_eq!(parts[12].offset, 12 * MIN_PART_SIZE);
        assert_eq!(parts[12].length, 100 * MB - 12 * MIN_PART_SIZE);
    }

    #[test]
    fn large_files_switch_to_the_divisor() {
        // 8GB / 1000 = 8.59MB, which is above the 8MB floor, so the divisor
        // takes over and the part count stays pinned at 1000.
        let total = 8 * GB;
        assert!(part_size_for(total) > MIN_PART_SIZE);
        assert_eq!(parts_of(&plan_upload(total)).len(), 1000);
    }

    #[test]
    fn part_size_is_clamped_to_the_s3_maximum() {
        // 5TB (S3's max object size) / 1000 = 5.12GB, which exceeds the 5GB
        // per-part ceiling; without the clamp the server would reject the
        // upload with EntityTooLarge.
        assert_eq!(part_size_for(5 * TB), MAX_PART_SIZE);
        assert!(parts_of(&plan_upload(5 * TB)).len() as u64 <= MAX_PARTS);
    }

    #[test]
    fn plans_are_internally_consistent() {
        let sizes = [
            MULTIPART_THRESHOLD,
            MULTIPART_THRESHOLD + 1,
            17 * MB,
            64 * MB,
            100 * MB,
            1023 * MB,
            7 * GB,
            8 * GB,
            64 * GB,
            5 * TB,
        ];
        for total in sizes {
            let plan = plan_upload(total);
            let parts = parts_of(&plan);
            let UploadPlan::Multipart { part_size, .. } = plan else {
                unreachable!()
            };

            assert!(!parts.is_empty(), "{total}: empty plan");
            assert!(parts.len() as u64 <= MAX_PARTS, "{total}: too many parts");
            assert_eq!(parts[0].offset, 0, "{total}: first part must start at 0");

            let mut expected_offset = 0u64;
            for (i, part) in parts.iter().enumerate() {
                assert_eq!(
                    part.number as usize,
                    i + 1,
                    "{total}: part numbers must be 1..=n"
                );
                assert_eq!(part.offset, expected_offset, "{total}: gap or overlap");
                assert!(part.length > 0, "{total}: zero-length part");
                if i + 1 < parts.len() {
                    assert_eq!(
                        part.length, part_size,
                        "{total}: only the last part may be short"
                    );
                } else {
                    assert!(part.length <= part_size, "{total}: last part too long");
                }
                expected_offset += part.length;
            }
            assert_eq!(
                expected_offset, total,
                "{total}: lengths must sum to the total"
            );
        }
    }
}
