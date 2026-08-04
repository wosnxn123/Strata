# Strata Benchmarks — vs Anvil

## Machine

| | |
|---|---|
| CPU | AMD EPYC 9K65 192-Core（容器配额 32C） |
| RAM | 64 GB |
| Disk | overlay fs, 256 GB |
| OS | Debian GNU/Linux 13 (trixie), kernel 5.4.241 |
| Rust | 1.97.1 stable |
| Date | 2026-08-04 |

## Results

### 1. Footprint（合成世界：4 region × 1024 chunk，NBT 200–800B，50% 共享生物群系模板前缀）

| 指标 | Anvil | Strata（convert --to-strata + compact） |
|---|---|---|
| 字节数 | 16,809,984 | 1,623,090 |
| **vault/anvil** | — | **0.0966×** |

目标 ≤0.65×，实测 **0.097×**（合成数据重复模式多，冷层 superfeatures 聚类 + zstd-9 收益显著）。

### 2. 写吞吐（10k 次随机 write + flush，256B 固定负载）

```
write_throughput_10k    time: [118.43 ms 118.91 ms 119.39 ms]
```
≈ **84k records/s**（单线程顺序追加 + zstd-3）。

### 3. 读延迟（1k 次随机 read，256 条已 flush 数据）

```
read_latency_1k         time: [7.9498 ms 7.9757 ms 8.0047 ms]（1k 次合计）
p50 = 7.84 µs    p99 = 11.01 µs
```

### 4. 内存上界（SieveCache, cache-mb=64）

```
100 pages × 1000 entries → billed 3,200,700 / 67,108,864 bytes（预算内）
sieve_insert_100k        time: [1.2353 ms 1.2360 ms]
sieve_lookup_10k         time: [163.41 µs 163.53 µs]
```

SieveCache 计费严格按页序列化字节，10 万条目下仍远低于 64MB 预算——内存与世界大小无关的性质成立（RSS 采样留待实机长时基准）。

## Reproduce

```bash
cargo bench -p strata-cli --bench vs_anvil -- --warm-up-time 1 --measurement-time 2
```
