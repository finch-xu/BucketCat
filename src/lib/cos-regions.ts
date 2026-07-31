import type { RegionCatalog, RegionOption } from "./regions";

/**
 * 腾讯云 COS 的地区表，抄自官方《可用地域》文档
 * （https://cloud.tencent.com/document/product/436/6224，2026-07-31）。
 *
 * 域名规律统一为 `cos.{id}.myqcloud.com`，没有阿里云金融云那种不符规律的例外
 * —— 但**仍然只查表、不字符串拼接**（与 `oss-regions.ts` / `qiniu-regions.ts`
 * 同一条纪律）：这张表是唯一事实来源，将来腾讯云新增不符规律的地区时，拼接会
 * 静悄悄地生成一个不存在的域名。
 *
 * 全部域名已在 2026-07-31 逐个核对：对每个端点发一次未鉴权的 HTTPS `GET /`，
 * 20 个（含下面 TODO 里的三个金融云）一律返回 `403` —— 也就是真实的 COS 网关
 * 在拒绝匿名请求，而不是域名不存在。**不要用 DNS 查询代替这个核对**：本项目
 * 的开发机上 `dig` 会把任意域名解析到 `198.18.0.0/15`（代理软件的 fake-IP
 * DNS），连 `cos.ap-atlantis-9.myqcloud.com` 这种编出来的地域都「解析成功」。
 * 同一个编造地域在 HTTPS 探测下会以 TLS 握手失败告终，这才是能区分真假的对照。
 *
 * `hasInternalNetwork` 为 false。COS 确实有内网域名
 * （`cos-internal.{id}.tencentcos.cn`），但它只在腾讯云 VPC 内可达，而
 * BucketCat 是跑在用户自己机器上的桌面客户端 —— 把一个必然连不上的选项摆进
 * 下拉框，只会制造一个看起来像 bug 的失败。与 `qiniu-regions.ts` /
 * `b2-regions.ts` / `rainyun-regions.ts` 一致。
 *
 * ## 关于「一个连接只能看到一个地域」
 *
 * 后端 `provider::s3` 依赖这条性质，选地域时也请记住它：COS 的地域端点是**硬
 * 边界**，不是同一批桶的不同入口。2026-07-31 实测，账号里唯一那个 `ap-beijing`
 * 的桶，只有 `cos.ap-beijing.myqcloud.com` 和账号级的
 * `service.cos.myqcloud.com` 能列出来，`ap-guangzhou` / `ap-shanghai` /
 * `ap-nanjing` / `ap-chengdu` / `ap-hongkong` 一律返回 0 个桶。腾讯云自己的
 * 第三方应用接入文档也是这么写的：「在应用中只能在服务地址指定的地域创建或选择
 * 存储桶」。
 *
 * 所以这里和七牛**恰好相反**：七牛的 `ListBuckets` 是账号级全局的，一个连接靠
 * `RegionRouting` 就能管全部地区；COS 则是一个连接对应一个地域，要看别的地域的
 * 桶就得再建一条连接。选错地域的表现是「连接成功但一个桶都看不到」，不是报错。
 */
export const COS_REGIONS: RegionOption[] = [
  // --- 中国大陆 ---
  { id: "ap-beijing", label: "北京", endpoint: "cos.ap-beijing.myqcloud.com" },
  { id: "ap-nanjing", label: "南京", endpoint: "cos.ap-nanjing.myqcloud.com" },
  { id: "ap-shanghai", label: "上海", endpoint: "cos.ap-shanghai.myqcloud.com" },
  { id: "ap-guangzhou", label: "广州", endpoint: "cos.ap-guangzhou.myqcloud.com" },
  { id: "ap-chengdu", label: "成都", endpoint: "cos.ap-chengdu.myqcloud.com" },
  { id: "ap-chongqing", label: "重庆", endpoint: "cos.ap-chongqing.myqcloud.com" },
  // --- 港澳台及海外 ---
  { id: "ap-hongkong", label: "中国香港", endpoint: "cos.ap-hongkong.myqcloud.com" },
  { id: "ap-singapore", label: "新加坡", endpoint: "cos.ap-singapore.myqcloud.com" },
  { id: "ap-jakarta", label: "雅加达", endpoint: "cos.ap-jakarta.myqcloud.com" },
  { id: "ap-seoul", label: "首尔", endpoint: "cos.ap-seoul.myqcloud.com" },
  { id: "ap-bangkok", label: "曼谷", endpoint: "cos.ap-bangkok.myqcloud.com" },
  { id: "ap-tokyo", label: "东京", endpoint: "cos.ap-tokyo.myqcloud.com" },
  { id: "me-saudi-arabia", label: "利雅得", endpoint: "cos.me-saudi-arabia.myqcloud.com" },
  { id: "na-siliconvalley", label: "硅谷", endpoint: "cos.na-siliconvalley.myqcloud.com" },
  { id: "na-ashburn", label: "弗吉尼亚", endpoint: "cos.na-ashburn.myqcloud.com" },
  { id: "sa-saopaulo", label: "圣保罗", endpoint: "cos.sa-saopaulo.myqcloud.com" },
  { id: "eu-frankfurt", label: "法兰克福", endpoint: "cos.eu-frankfurt.myqcloud.com" },

  // TODO(金融云)：腾讯云另有三个金融云地域，域名同样符合 `cos.{id}.myqcloud.com`
  // 规律，数据现成：
  //
  //   { id: "ap-shanghai-fsi", label: "上海金融", endpoint: "cos.ap-shanghai-fsi.myqcloud.com" },
  //   { id: "ap-shenzhen-fsi", label: "深圳金融", endpoint: "cos.ap-shenzhen-fsi.myqcloud.com" },
  //   { id: "ap-beijing-fsi",  label: "北京金融", endpoint: "cos.ap-beijing-fsi.myqcloud.com"  },
  //
  // 要不要放进来是个产品判断，不是技术判断 —— 金融云是独立开通的产品，普通账号
  // 选中后必然连不上，用户会得到一个看起来像 bug 的失败。三条路：
  //
  //   1. 不放（当前状态）。目录只列普通账号选得中的地域，选中即可用；金融云用户
  //      走「通用 S3 兼容」手填 endpoint。
  //   2. 放进来，并给 `COS_CATALOG` 加 `groups`，复用 `oss-regions.ts` 已有的
  //      `addConn.regionGroupPublic` / `addConn.regionGroupFinance` 两个 i18n
  //      key（无需新增翻译），同时给上面 17 条各加 `group: "public"`。分组标题
  //      本身就是那句说明，把失败前置成一行字。
  //   3. 放进来但不分组 —— 不推荐，「上海金融」和「上海」并排会被当成同一朵云。
];

export const COS_CATALOG: RegionCatalog = {
  regions: COS_REGIONS,
  hasInternalNetwork: false,
};
