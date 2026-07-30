import type { RegionCatalog, RegionOption } from "./regions";

/**
 * 七牛云 Kodo 的 S3 兼容地区表，抄自官方《S3 域名》文档
 * （https://developer.qiniu.com/kodo/4088/s3-access-domainname，2026-07-30）。
 *
 * 八个域名已在 2026-07-30 全量核对：DNS 均解析到真实的七牛网关，
 * `cn-east-1` / `cn-east-2` / `cn-north-1` / `cn-south-1` / `ap-southeast-1`
 * 还实际收到过 `ListBuckets` 的 200。命名规律统一为 `s3.{id}.qiniucs.com`，
 * 没有阿里云金融云那种不符规律的例外 —— 但**仍然只查表、不字符串拼接**
 * （与 `oss-regions.ts` 同一条纪律）：这张表是唯一事实来源，将来七牛新增
 * 不符规律的地区时，拼接会静悄悄地生成一个不存在的域名。
 *
 * 七牛的 S3 域名文档只有一列公网域名，没有内网/VPC 形式，所以
 * `hasInternalNetwork` 为 false，`RegionOption.internal` 一律缺省。
 *
 * 也刻意不分组（没有 `groups`）：八项平铺已经够看，而 OSS 的分组语义是
 * 账号类型（公共云/金融云/政务云）而非地理位置，给七牛加「中国大陆/海外」
 * 这种地理分组会和既有约定打架。
 *
 * 注意一个七牛特有的性质（后端 `provider::s3` 依赖它）：`ListBuckets` 是
 * **账号级全局**的，任一地区的域名都会返回账号下所有地区的桶；但对某个桶做
 * 对象操作时必须命中该桶所在地区的域名，否则得到 `400 IncorrectRegion`。
 * 跨地区路由由后端的 `RegionRouting` 自动完成，所以一个连接就够管全部地区。
 */
export const QINIU_REGIONS: RegionOption[] = [
  { id: "cn-east-1", label: "华东-浙江", endpoint: "s3.cn-east-1.qiniucs.com" },
  { id: "cn-east-2", label: "华东-浙江2", endpoint: "s3.cn-east-2.qiniucs.com" },
  { id: "cn-north-1", label: "华北-河北", endpoint: "s3.cn-north-1.qiniucs.com" },
  { id: "cn-south-1", label: "华南-广东", endpoint: "s3.cn-south-1.qiniucs.com" },
  { id: "us-north-1", label: "北美-洛杉矶", endpoint: "s3.us-north-1.qiniucs.com" },
  { id: "ap-southeast-1", label: "亚太-新加坡", endpoint: "s3.ap-southeast-1.qiniucs.com" },
  { id: "ap-southeast-2", label: "亚太-河内", endpoint: "s3.ap-southeast-2.qiniucs.com" },
  { id: "ap-southeast-3", label: "亚太-胡志明", endpoint: "s3.ap-southeast-3.qiniucs.com" },
];

export const QINIU_CATALOG: RegionCatalog = {
  regions: QINIU_REGIONS,
  hasInternalNetwork: false,
};
