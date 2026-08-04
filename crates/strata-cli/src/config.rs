//! `strata.properties` 配置：模板创建、Java-properties 风格解析与配置矩阵校验。
//!
//! 文件位于世界根目录（`<world>/strata.properties`）。解析规则：
//! - `#` / `!` 开头的整行为注释；
//! - 行尾 `\` 表示续行（逻辑行行号取首个物理行）；
//! - 按首个 `=` 分割 key/value，两侧 trim；
//! - 文件开头 U+FEFF BOM 自动剥离（Windows 记事本产物）；
//! - 未知 key 忽略（eprintln 告警）；
//! - 非法值 → [`StrataError::Config`]。
//!
//! **短路规则**：显式 `strata.enabled=false` → 跳过其余键的校验，直接返回
//! 默认 [`StrataConfig`]（enabled=false）。

use std::path::Path;

use strata_core::gc::GcConfig;
use strata_core::store::StoreConfig;
use strata_core::tier::TierConfig;
use strata_core::StrataError;

/// 配置文件名（位于世界根目录）。
pub const CONFIG_FILE: &str = "strata.properties";

/// 无配置文件时写入的模板（默认 `strata.enabled=false`；与 Java 侧逐字节同款）。
const TEMPLATE: &str = "# Strata storage configuration / Strata 存储配置
# Master switch (default off — opt in) / 总开关（默认关闭，需显式启用）
strata.enabled=false
# Cold tier (hot -> cold migration) / 冷层（热→冷迁移）
strata.tiering.enabled=true
strata.tiering.stable-flushes=30
strata.tiering.invalid-demote-ratio=0.25
# Compression / 压缩
strata.compression.hot-enabled=true
strata.compression.cold-enabled=true
strata.compression.hot=zstd-3
strata.compression.cold=zstd-9
# Batch compression workers: 0=auto(all cores) 1=serial(default, TPS-first) N>=2=capped
# 批量压缩线程：0=自动(全核) 1=串行(默认,TPS优先) N≥2=限N线程
strata.compression.threads=1
# Index memory budget (MiB) / 索引内存预算（MiB）
strata.index.cache-mb=512
# GC / 垃圾回收
strata.gc.enabled=true
strata.gc.invalid-threshold=0.6
strata.gc.budget-bytes=33554432
# Minimum hole bytes for punch / 挖洞最小洞阈值（字节）
strata.gc.min-hole-bytes=65536
# Emergency escape: boot on Anvil even when a vstore exists (data in vstore invisible until converted back)
# 应急逃生门：vstore 存在时仍按 Anvil 启动（vstore 内数据在转回前不可见）
strata.force-anvil=false
";

/// CLI 侧顶层配置。
#[derive(Debug, Clone)]
pub struct StrataConfig {
    /// 是否启用 Strata 存储引擎（false = 回退 Anvil）。
    pub enabled: bool,
    pub tier: TierConfig,
    pub store: StoreConfig,
    pub gc: GcConfig,
    /// `strata.gc.enabled`：false 时 compact 跳过 GC 阶段（模板默认 true）。
    pub gc_enabled: bool,
    /// `strata.force-anvil`：Java 运行时逃生门键；CLI 只解析不使用。
    pub force_anvil: bool,
}

impl Default for StrataConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tier: TierConfig::default(),
            store: StoreConfig::default(),
            gc: GcConfig::default(),
            // 与模板取值一致：GC 默认开，逃生门默认关。
            gc_enabled: true,
            force_anvil: false,
        }
    }
}



// TierConfig/StoreConfig/GcConfig 只派生 Debug+Clone，这里按公开字段逐一比较。
impl PartialEq for StrataConfig {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.tier.enabled == other.tier.enabled
            && self.tier.stable_flushes == other.tier.stable_flushes
            && self.tier.invalid_demote_ratio == other.tier.invalid_demote_ratio
            && self.store.hot_level == other.store.hot_level
            && self.store.hot_enabled == other.store.hot_enabled
            && self.store.cold_level == other.store.cold_level
            && self.store.cold_enabled == other.store.cold_enabled
            && self.store.dictionary == other.store.dictionary
            && self.store.cache_mb == other.store.cache_mb
            && self.store.segment_max_bytes == other.store.segment_max_bytes
            && self.store.compression_threads == other.store.compression_threads
            && self.gc.invalid_threshold == other.gc.invalid_threshold
            && self.gc.budget_bytes == other.gc.budget_bytes
            && self.gc.min_hole_bytes == other.gc.min_hole_bytes
            && self.gc_enabled == other.gc_enabled
            && self.force_anvil == other.force_anvil
    }
}

/// 读取世界根目录下的配置；文件不存在时先写入模板，再返回默认配置。
pub fn load_or_create_template(world_root: &Path) -> Result<StrataConfig, StrataError> {
    let path = world_root.join(CONFIG_FILE);
    if !path.exists() {
        std::fs::write(&path, TEMPLATE).map_err(|e| StrataError::Config {
            file: CONFIG_FILE.to_string(),
            line: 0,
            detail: format!("写入配置模板失败: {e}"),
        })?;
        return Ok(StrataConfig::default());
    }
    let text = std::fs::read_to_string(&path)?;
    parse(&text)
}

/// 配置矩阵校验：返回 WARN 文案列表（供 CLI 打到 stderr）。
pub fn validate_matrix(cfg: &StrataConfig) -> Vec<String> {
    let mut warns = Vec::new();
    if !cfg.enabled {
        warns.push("strata.enabled=false，存储引擎未启用（回退 Anvil）".to_string());
        return warns;
    }
    if !cfg.tier.enabled {
        warns.push("冷层已关闭，忽略冷层配置".to_string());
    }
    if !cfg.store.hot_enabled && !cfg.store.cold_enabled {
        warns.push("压缩全部关闭，存档体积将显著增大".to_string());
    } else if cfg.store.hot_enabled && !cfg.store.cold_enabled {
        warns.push("冷层未启用压缩，体积可能增大".to_string());
    }
    warns
}

/// 构造 `StrataError::Config`（file 恒为 [`CONFIG_FILE`]）。
fn bad(line: u32, detail: impl Into<String>) -> StrataError {
    StrataError::Config {
        file: CONFIG_FILE.into(),
        line,
        detail: detail.into(),
    }
}

/// 首个 `=` 分割 key/value，两侧 trim；无 `=` 或空 key → `None`。
fn split_kv(body: &str) -> Option<(String, String)> {
    let (k, v) = body.split_once('=')?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.trim().to_string()))
}

fn parse_bool(key: &str, value: &str, line: u32) -> Result<bool, StrataError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(bad(
            line,
            format!("{key}: 无法把 '{value}' 解析为 bool（期望 true/false）"),
        )),
    }
}

/// 解析 `zstd-<N>`（N ∈ [-10, 22] 且 N != 0）；热层额外接受 `none`。
fn apply_codec(cfg: &mut StrataConfig, key: &str, value: &str, line: u32, hot: bool) -> Result<(), StrataError> {
    if hot && value == "none" {
        cfg.store.hot_enabled = false;
        return Ok(());
    }
    let rest = value.strip_prefix("zstd-").ok_or_else(|| {
        bad(
            line,
            format!("{key}: 非法压缩配置 '{value}'（期望 zstd-<N>，热层可为 none）"),
        )
    })?;
    let level: i32 = rest.parse().map_err(|_| {
        bad(
            line,
            format!("{key}: 无法把 '{value}' 的级别解析为整数"),
        )
    })?;
    if level == 0 || !(-10..=22).contains(&level) {
        return Err(bad(
            line,
            format!("{key}: zstd 级别 {level} 非法（要求 [-10, 22] 且 != 0）"),
        ));
    }
    if hot {
        cfg.store.hot_level = level;
        cfg.store.hot_enabled = true;
    } else {
        cfg.store.cold_level = level;
        cfg.store.cold_enabled = true;
    }
    Ok(())
}

/// 把物理行连成逻辑行（行尾 `\` 续行），记录起始物理行号（1 基）。
fn logical_lines(text: &str) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut iter = text.lines().enumerate().peekable();
    while let Some((idx, raw)) = iter.next() {
        let line_no = (idx + 1) as u32;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        let mut buf = String::new();
        let mut cur = raw;
        loop {
            match cur.trim_end().strip_suffix('\\') {
                Some(stripped) => {
                    buf.push_str(stripped);
                    match iter.next() {
                        Some((_, nxt)) => cur = nxt,
                        None => break,
                    }
                }
                None => {
                    buf.push_str(cur);
                    break;
                }
            }
        }
        out.push((line_no, buf));
    }
    out
}

/// 解析配置文件全文。
fn parse(text: &str) -> Result<StrataConfig, StrataError> {
    // 文件开头可能带 UTF-8 BOM（Windows 记事本），不剥离会让首个键无法识别。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let logical = logical_lines(text);

    // 第一遍：显式 strata.enabled=false → 短路，跳过其余键的校验。
    let mut enabled_explicit: Option<bool> = None;
    for &(line, ref body) in &logical {
        if let Some((key, value)) = split_kv(body) {
            if key == "strata.enabled" {
                enabled_explicit = Some(parse_bool(&key, &value, line)?);
            }
        }
    }
    if enabled_explicit == Some(false) {
        return Ok(StrataConfig::default());
    }

    let mut cfg = StrataConfig {
        enabled: enabled_explicit.unwrap_or(false),
        ..StrataConfig::default()
    };

    for &(line, ref body) in &logical {
        let Some((key, value)) = split_kv(body) else {
            continue;
        };
        match key.as_str() {
            "strata.enabled" => {} // 已在第一遍处理
            "strata.tiering.enabled" => {
                cfg.tier.enabled = parse_bool(&key, &value, line)?;
            }
            "strata.tiering.stable-flushes" => {
                cfg.tier.stable_flushes = value.parse::<u32>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 u32"))
                })?;
            }
            "strata.tiering.invalid-demote-ratio" => {
                cfg.tier.invalid_demote_ratio = value.parse::<f64>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 f64"))
                })?;
            }
            "strata.compression.hot" => apply_codec(&mut cfg, &key, &value, line, true)?,
            "strata.compression.cold" => apply_codec(&mut cfg, &key, &value, line, false)?,
            "strata.compression.hot-enabled" => {
                cfg.store.hot_enabled = parse_bool(&key, &value, line)?;
            }
            "strata.compression.cold-enabled" => {
                cfg.store.cold_enabled = parse_bool(&key, &value, line)?;
            }
            // 已停用（全链路无生产者无消费者）：仍识别旧配置中的键，校验后告警忽略，不落存储。
            "strata.compression.dictionary" => {
                parse_bool(&key, &value, line)?;
                eprintln!(
                    "WARN: strata.compression.dictionary 已停用（字典压缩未接线），忽略"
                );
            }
            "strata.compression.threads" => {
                cfg.store.compression_threads = value.parse::<u32>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 u32"))
                })?;
            }
            "strata.index.cache-mb" => {
                cfg.store.cache_mb = value.parse::<u64>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 u64"))
                })?;
            }
            "strata.gc.enabled" => {
                cfg.gc_enabled = parse_bool(&key, &value, line)?;
            }
            "strata.gc.invalid-threshold" => {
                cfg.gc.invalid_threshold = value.parse::<f64>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 f64"))
                })?;
            }
            "strata.gc.budget-bytes" => {
                cfg.gc.budget_bytes = value.parse::<u64>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 u64"))
                })?;
            }
            "strata.gc.min-hole-bytes" => {
                cfg.gc.min_hole_bytes = value.parse::<u64>().map_err(|_| {
                    bad(line, format!("{key}: 无法把 '{value}' 解析为 u64"))
                })?;
            }
            // Java 运行时逃生门：CLI 自身不使用，只解析以免报未知键告警。
            "strata.force-anvil" => {
                cfg.force_anvil = parse_bool(&key, &value, line)?;
            }
            other => {
                eprintln!("WARN: {CONFIG_FILE} 中未知配置项 '{other}'，已忽略");
            }
        }
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_creates_template_with_disabled_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_or_create_template(dir.path()).unwrap();
        assert_eq!(cfg, StrataConfig::default());
        assert!(!cfg.enabled);

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.starts_with("# Strata storage configuration"));
        assert!(text.contains("strata.enabled=false"));
        assert!(text.contains("strata.compression.hot=zstd-3"));
        // 最终模板形态：新增键在位、dictionary 已下线。
        assert!(text.contains("strata.gc.min-hole-bytes=65536"));
        assert!(text.contains("strata.force-anvil=false"));
        assert!(text.contains("strata.gc.enabled=true"));
        assert!(!text.contains("strata.compression.dictionary"));

        // 再次加载：读回模板，显式 enabled=false → 仍是默认配置。
        let cfg2 = load_or_create_template(dir.path()).unwrap();
        assert_eq!(cfg2, StrataConfig::default());
    }

    #[test]
    fn level_bounds_enforced() {
        for bad_value in ["zstd-0", "zstd-23", "zstd--11", "lz4"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(CONFIG_FILE),
                format!("strata.compression.hot={bad_value}\n"),
            )
            .unwrap();
            let err = load_or_create_template(dir.path()).unwrap_err();
            match err {
                StrataError::Config { file, line, .. } => {
                    assert_eq!(file, CONFIG_FILE, "{bad_value}");
                    assert_eq!(line, 1, "{bad_value}");
                }
                other => panic!("expected Config error for {bad_value}, got {other:?}"),
            }
        }
    }

    #[test]
    fn matrix_warns_on_all_compression_off() {
        let cfg = StrataConfig {
            enabled: true,
            store: StoreConfig {
                hot_enabled: false,
                cold_enabled: false,
                ..StoreConfig::default()
            },
            ..StrataConfig::default()
        };
        let warns = validate_matrix(&cfg);
        assert!(warns.iter().any(|w| w.contains("压缩全部关闭")));
    }

    #[test]
    fn disabled_ignores_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "strata.enabled=false\nstrata.compression.hot=lz4\nstrata.gc.budget-bytes=oops\n",
        )
        .unwrap();
        let cfg = load_or_create_template(dir.path()).unwrap();
        assert_eq!(cfg, StrataConfig::default());
    }

    #[test]
    fn dictionary_key_deprecated_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "strata.enabled=true\nstrata.compression.dictionary=false\n",
        )
        .unwrap();
        // 弃用键只告警不落存储：保持 StoreConfig 默认值。
        let cfg = load_or_create_template(dir.path()).unwrap();
        assert_eq!(cfg.store.dictionary, StoreConfig::default().dictionary);
    }

    #[test]
    fn gc_and_force_anvil_keys_parsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "strata.enabled=true\nstrata.gc.enabled=false\n\
             strata.gc.min-hole-bytes=12345\nstrata.force-anvil=true\n",
        )
        .unwrap();
        let cfg = load_or_create_template(dir.path()).unwrap();
        assert!(!cfg.gc_enabled);
        assert_eq!(cfg.gc.min_hole_bytes, 12345);
        assert!(cfg.force_anvil);
    }

    #[test]
    fn min_hole_bytes_rejects_non_u64() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "strata.gc.min-hole-bytes=oops\n",
        )
        .unwrap();
        let err = load_or_create_template(dir.path()).unwrap_err();
        match err {
            StrataError::Config { line, .. } => assert_eq!(line, 1),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
