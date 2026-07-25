import type { RegionCatalog, RegionOption } from "./regions";

/**
 * Rainyun ROS region table.
 *
 * Endpoints verified live on 2026-07-25. Each region is a SEPARATE MinIO
 * cluster with its OWN credentials -- an access key issued for `cn-nb1` gets
 * `InvalidAccessKeyId` on `cn-sy1`. One connection therefore binds to
 * exactly one region; a user with buckets in both must create two
 * connections. (This is the opposite of Aliyun OSS, where one key spans
 * every region.)
 *
 * Rainyun has no VPC/internal endpoints and no region grouping, so
 * `RegionOption.internal` / `.group` are absent and `hasInternalNetwork` is
 * false.
 */
export const RAINYUN_REGIONS: RegionOption[] = [
  { id: "cn-nb1", label: "浙江宁波", endpoint: "cn-nb1.rains3.com" },
  { id: "cn-sy1", label: "湖北十堰", endpoint: "cn-sy1.rains3.com" },
];

export const RAINYUN_CATALOG: RegionCatalog = {
  regions: RAINYUN_REGIONS,
  hasInternalNetwork: false,
};
