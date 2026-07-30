import type { RegionCatalog, RegionOption } from "./regions";

/**
 * Backblaze B2 的 S3 兼容地区表。
 *
 * 七个域名已在 2026-07-31 全量核对：DNS 逐个探测，`us-west-000/001/002/004`、
 * `us-east-005`、`eu-central-003`、`ca-east-006` 全部解析到真实的 Backblaze
 * 网关，而 `us-west-003`、`ca-central-001` 以及一个随手编的地区名都是
 * NXDOMAIN —— 即 Backblaze **没有泛域名解析**，所以解析成功的这七个就是真实
 * 存在的全部。`us-west-004` 还实际收到过 `ListBuckets` / `ListObjectsV2` /
 * `HeadBucket` 的 200。
 *
 * 命名规律统一为 `s3.{id}.backblazeb2.com`，但**仍然只查表、不字符串拼接**
 * （与 `oss-regions.ts` / `qiniu-regions.ts` 同一条纪律）：这张表是唯一事实
 * 来源，将来 Backblaze 新增不符规律的地区时，拼接会静悄悄地生成一个不存在的
 * 域名。
 *
 * ## label 为什么这么写
 *
 * 前三张表（OSS / 七牛 / 雨云）用的都是厂商自己公布的中文地区名，B2 是第一个
 * 拿到地区目录的非中国厂商，沿用同一条约定 —— 用厂商自己公布的名字，也就是
 * 英文。
 *
 * 但只标 Backblaze **官方明确写出来**的：官方的《数据区域》文档说 US West 有
 * Sacramento 和 Phoenix 两个数据中心，却从未说明 `000/001/002/004` 这四个集群
 * 各自落在哪一个（网上流传的对应关系彼此矛盾，有一版甚至给出了官方文档里根本
 * 没有的城市）。所以四个 US West 集群一律只写 "US West"，靠 `RegionPicker`
 * 渲染的 `${label} · ${id}` 里的 id 后缀区分；只有单集群的三个地区才敢标城市。
 * 宁可少写，不编造。
 *
 * ## 没有分组、没有内网
 *
 * B2 没有内网/VPC 端点，所以 `hasInternalNetwork` 为 false、`internal` 一律
 * 缺省。也刻意不分组：这张表在 UI 里只是「keyID 推导失败时的手动退路」，本就
 * 罕见，七项平铺已经够看（与 `qiniu-regions.ts` 同一判断）。
 *
 * ## 一个 B2 特有的性质：地区是账号级固定的
 *
 * 用户无法选择自己在哪个地区 —— 账号在哪个 realm 创建，桶就永远在那。而且选错
 * 地区的报错极具误导性：2026-07-30 实测，拿 `004` 集群的密钥去打
 * `s3.us-east-005.backblazeb2.com`，返回的是 `403 InvalidAccessKeyId
 * "The key ... is not valid"`。用户看到「密钥无效」会跑去反复重建密钥，永远
 * 查不到真因。这正是 `b2RegionFromKeyId` 存在的理由：能推导出来就别让用户选。
 */

/** 一个 B2 地区。比 `RegionOption` 多一个集群号，见 `b2RegionFromKeyId`。 */
export interface B2Region extends RegionOption {
  /** keyID 开头三位的集群号。
   *
   * **Backblaze 官方从未文档化这条对应关系** —— application keys 文档和 master
   * key 帮助页都查过，只字未提 keyID 的格式。它是逆向观察出来的：实测一把真实
   * 密钥 `004024147e7d0760000000001` = `004` + accountId `024147e7d076` +
   * `0000000001`，而同一把密钥的 `b2_authorize_account` 返回
   * `s3ApiUrl: https://s3.us-west-004.backblazeb2.com`，用户控制台显示的
   * Endpoint 也是同一个（三方互证）；官方 API 文档的示例密钥 `0027464dd...`
   * 同样与 `us-west-002` 吻合。
   *
   * 因此这个字段**只用于即时预览**，最终以 `b2_probe_key`（官方
   * `b2_authorize_account`）返回的 `s3ApiUrl` 为准 —— 那条路径既权威，又能在
   * Backblaze 将来新开地区时不发版就支持。 */
  cluster: string;
}

export const B2_REGIONS: B2Region[] = [
  { id: "us-west-000", cluster: "000", label: "US West", endpoint: "s3.us-west-000.backblazeb2.com" },
  { id: "us-west-001", cluster: "001", label: "US West", endpoint: "s3.us-west-001.backblazeb2.com" },
  { id: "us-west-002", cluster: "002", label: "US West", endpoint: "s3.us-west-002.backblazeb2.com" },
  { id: "eu-central-003", cluster: "003", label: "EU Central (Amsterdam)", endpoint: "s3.eu-central-003.backblazeb2.com" },
  { id: "us-west-004", cluster: "004", label: "US West", endpoint: "s3.us-west-004.backblazeb2.com" },
  { id: "us-east-005", cluster: "005", label: "US East (Virginia)", endpoint: "s3.us-east-005.backblazeb2.com" },
  { id: "ca-east-006", cluster: "006", label: "CA East (Toronto)", endpoint: "s3.ca-east-006.backblazeb2.com" },
];

export const B2_CATALOG: RegionCatalog = {
  regions: B2_REGIONS,
  hasInternalNetwork: false,
};

/** 主应用密钥 ID 的形状：裸 accountId，纯 12 位十六进制、没有集群号前缀。
 *
 * 长度是这里唯一可靠的判据，所以 `b2RegionFromKeyId` 也用它做闸门。 */
const MASTER_KEY_ID_PATTERN = /^[0-9a-f]{12}$/i;

/**
 * keyID 是否长得像**主**应用密钥 ID。
 *
 * Backblaze 官方明文规定主密钥不能用于 S3 兼容 API（*"You cannot use your
 * master application key with the S3-Compatible API"*），而实测拿它去调 S3 得到
 * 的是 `403 InvalidAccessKeyId "Malformed Access Key Id"` —— 又一条会让用户
 * 跑偏的报错。表单据此提前给出针对性提示，而不是等用户撞上那句话。
 *
 * 判断刻意保守：只认「纯 12 位 hex」这一种确定形状，拿不准就返回 false，让
 * 用户照常提交（真错了服务端会拒绝），绝不因为猜测而拦住一把合法密钥。
 */
export function looksLikeB2MasterKeyId(keyId: string): boolean {
  return MASTER_KEY_ID_PATTERN.test(keyId.trim());
}

/**
 * 从 keyID 推导地区。查表，不做字符串拼接。
 *
 * 返回 `undefined` 表示推导不出来 —— 输入残缺、用了主密钥、或者 Backblaze
 * 开了这张表还不知道的新集群。三种情况调用方都该回落到手动选择，而不是猜。
 *
 * 长度闸门（`> 12`）不是可有可无的：主密钥 ID 是 12 位 hex，其中完全可能出现
 * `004...` 这样以三位数字开头的账号 id，只看前缀会把它错认成 us-west-004。
 */
export function b2RegionFromKeyId(keyId: string): B2Region | undefined {
  const id = keyId.trim();
  if (id.length <= 12) return undefined;
  const cluster = id.slice(0, 3);
  if (!/^\d{3}$/.test(cluster)) return undefined;
  return B2_REGIONS.find((r) => r.cluster === cluster);
}
