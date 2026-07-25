/**
 * Aliyun OSS region/endpoint reference table, transcribed verbatim from the
 * official "OSS 地域和访问域名" documentation (as of 2026-07-25).
 *
 * IMPORTANT: endpoints must always be looked up from this table, never
 * derived by string concatenation. Most regions follow the
 * `oss-{id}.aliyuncs.com` / `oss-{id}-internal.aliyuncs.com` pattern, but the
 * finance-cloud entries do NOT -- e.g. `cn-hangzhou-finance` resolves to
 * `oss-cn-hzfinance.aliyuncs.com`, not `oss-cn-hangzhou-finance.aliyuncs.com`.
 * Building endpoints by concatenating `oss-${id}.aliyuncs.com` would silently
 * produce a wrong (unreachable) host for every finance-cloud region.
 */
import type { RegionCatalog, RegionOption } from "./regions";

export interface OssRegion extends RegionOption {
  /** Internal (VPC) endpoint, no scheme. Always present for OSS. */
  internal: string;
  group: "public" | "finance" | "gov";
}

export const OSS_REGIONS: OssRegion[] = [
  { id: "cn-hangzhou", label: "华东1（杭州）", endpoint: "oss-cn-hangzhou.aliyuncs.com", internal: "oss-cn-hangzhou-internal.aliyuncs.com", group: "public" },
  { id: "cn-shanghai", label: "华东2（上海）", endpoint: "oss-cn-shanghai.aliyuncs.com", internal: "oss-cn-shanghai-internal.aliyuncs.com", group: "public" },
  { id: "cn-nanjing", label: "华东5（南京·本地地域·关停中）", endpoint: "oss-cn-nanjing.aliyuncs.com", internal: "oss-cn-nanjing-internal.aliyuncs.com", group: "public" },
  { id: "cn-fuzhou", label: "华东6（福州·本地地域·关停中）", endpoint: "oss-cn-fuzhou.aliyuncs.com", internal: "oss-cn-fuzhou-internal.aliyuncs.com", group: "public" },
  { id: "cn-wuhan-lr", label: "华中1（武汉·本地地域）", endpoint: "oss-cn-wuhan-lr.aliyuncs.com", internal: "oss-cn-wuhan-lr-internal.aliyuncs.com", group: "public" },
  { id: "cn-qingdao", label: "华北1（青岛）", endpoint: "oss-cn-qingdao.aliyuncs.com", internal: "oss-cn-qingdao-internal.aliyuncs.com", group: "public" },
  { id: "cn-beijing", label: "华北2（北京）", endpoint: "oss-cn-beijing.aliyuncs.com", internal: "oss-cn-beijing-internal.aliyuncs.com", group: "public" },
  { id: "cn-zhangjiakou", label: "华北3（张家口）", endpoint: "oss-cn-zhangjiakou.aliyuncs.com", internal: "oss-cn-zhangjiakou-internal.aliyuncs.com", group: "public" },
  { id: "cn-huhehaote", label: "华北5（呼和浩特）", endpoint: "oss-cn-huhehaote.aliyuncs.com", internal: "oss-cn-huhehaote-internal.aliyuncs.com", group: "public" },
  { id: "cn-wulanchabu", label: "华北6（乌兰察布）", endpoint: "oss-cn-wulanchabu.aliyuncs.com", internal: "oss-cn-wulanchabu-internal.aliyuncs.com", group: "public" },
  { id: "cn-shenzhen", label: "华南1（深圳）", endpoint: "oss-cn-shenzhen.aliyuncs.com", internal: "oss-cn-shenzhen-internal.aliyuncs.com", group: "public" },
  { id: "cn-heyuan", label: "华南2（河源）", endpoint: "oss-cn-heyuan.aliyuncs.com", internal: "oss-cn-heyuan-internal.aliyuncs.com", group: "public" },
  { id: "cn-guangzhou", label: "华南3（广州）", endpoint: "oss-cn-guangzhou.aliyuncs.com", internal: "oss-cn-guangzhou-internal.aliyuncs.com", group: "public" },
  { id: "cn-chengdu", label: "西南1（成都）", endpoint: "oss-cn-chengdu.aliyuncs.com", internal: "oss-cn-chengdu-internal.aliyuncs.com", group: "public" },
  { id: "cn-zhongwei", label: "西北2（中卫）", endpoint: "oss-cn-zhongwei.aliyuncs.com", internal: "oss-cn-zhongwei-internal.aliyuncs.com", group: "public" },
  { id: "cn-hongkong", label: "中国香港", endpoint: "oss-cn-hongkong.aliyuncs.com", internal: "oss-cn-hongkong-internal.aliyuncs.com", group: "public" },
  { id: "ap-northeast-1", label: "日本（东京）", endpoint: "oss-ap-northeast-1.aliyuncs.com", internal: "oss-ap-northeast-1-internal.aliyuncs.com", group: "public" },
  { id: "ap-northeast-2", label: "韩国（首尔）", endpoint: "oss-ap-northeast-2.aliyuncs.com", internal: "oss-ap-northeast-2-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-1", label: "新加坡", endpoint: "oss-ap-southeast-1.aliyuncs.com", internal: "oss-ap-southeast-1-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-3", label: "马来西亚（吉隆坡）", endpoint: "oss-ap-southeast-3.aliyuncs.com", internal: "oss-ap-southeast-3-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-5", label: "印度尼西亚（雅加达）", endpoint: "oss-ap-southeast-5.aliyuncs.com", internal: "oss-ap-southeast-5-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-6", label: "菲律宾（马尼拉）", endpoint: "oss-ap-southeast-6.aliyuncs.com", internal: "oss-ap-southeast-6-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-7", label: "泰国（曼谷）", endpoint: "oss-ap-southeast-7.aliyuncs.com", internal: "oss-ap-southeast-7-internal.aliyuncs.com", group: "public" },
  { id: "ap-southeast-8", label: "马来西亚（柔佛州）", endpoint: "oss-ap-southeast-8.aliyuncs.com", internal: "oss-ap-southeast-8-internal.aliyuncs.com", group: "public" },
  { id: "eu-central-1", label: "德国（法兰克福）", endpoint: "oss-eu-central-1.aliyuncs.com", internal: "oss-eu-central-1-internal.aliyuncs.com", group: "public" },
  { id: "eu-west-1", label: "英国（伦敦）", endpoint: "oss-eu-west-1.aliyuncs.com", internal: "oss-eu-west-1-internal.aliyuncs.com", group: "public" },
  { id: "eu-west-2", label: "法国（巴黎）", endpoint: "oss-eu-west-2.aliyuncs.com", internal: "oss-eu-west-2-internal.aliyuncs.com", group: "public" },
  { id: "us-west-1", label: "美国（硅谷）", endpoint: "oss-us-west-1.aliyuncs.com", internal: "oss-us-west-1-internal.aliyuncs.com", group: "public" },
  { id: "us-east-1", label: "美国（弗吉尼亚）", endpoint: "oss-us-east-1.aliyuncs.com", internal: "oss-us-east-1-internal.aliyuncs.com", group: "public" },
  { id: "na-south-1", label: "墨西哥", endpoint: "oss-na-south-1.aliyuncs.com", internal: "oss-na-south-1-internal.aliyuncs.com", group: "public" },
  { id: "me-east-1", label: "阿联酋（迪拜）", endpoint: "oss-me-east-1.aliyuncs.com", internal: "oss-me-east-1-internal.aliyuncs.com", group: "public" },
  { id: "cn-hangzhou-finance", label: "华东1金融云", endpoint: "oss-cn-hzfinance.aliyuncs.com", internal: "oss-cn-hzfinance-internal.aliyuncs.com", group: "finance" },
  { id: "cn-shanghai-finance-1", label: "华东2金融云", endpoint: "oss-cn-shanghai-finance-1-pub.aliyuncs.com", internal: "oss-cn-shanghai-finance-1-pub-internal.aliyuncs.com", group: "finance" },
  { id: "cn-shenzhen-finance-1", label: "华南1金融云", endpoint: "oss-cn-szfinance.aliyuncs.com", internal: "oss-cn-szfinance-internal.aliyuncs.com", group: "finance" },
  { id: "cn-beijing-finance-1", label: "华北2金融云", endpoint: "oss-cn-beijing-finance-1-pub.aliyuncs.com", internal: "oss-cn-beijing-finance-1-pub-internal.aliyuncs.com", group: "finance" },
  { id: "cn-north-2-gov-1", label: "华北2政务云", endpoint: "oss-cn-north-2-gov-1.aliyuncs.com", internal: "oss-cn-north-2-gov-1-internal.aliyuncs.com", group: "gov" },
];

/** OSS 的区域分组，顺序即选择器里的显示顺序。 */
const OSS_REGION_GROUPS = [
  { key: "public", labelKey: "addConn.regionGroupPublic" },
  { key: "finance", labelKey: "addConn.regionGroupFinance" },
  { key: "gov", labelKey: "addConn.regionGroupGov" },
];

export const OSS_CATALOG: RegionCatalog = {
  regions: OSS_REGIONS,
  groups: OSS_REGION_GROUPS,
  hasInternalNetwork: true,
};
