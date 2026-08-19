//! Model-weight fit against a hardware profile's memory, with a visible reserve.
//!
//! The weights figure is the certain part: `total_parameters ×
//! bytes_per_parameter(quantization)`. KV-cache and runtime overhead are far
//! less certain, so they are reported as an [`OverheadRange`] band rather than a
//! single fabricated value. Pass/fail is evaluated on weights against the
//! device memory pool *after* a configurable [`FitConfig::reserve`].
//!
//! A multi-GPU node aggregates VRAM across its cards (`count × per-card`), with a
//! per-device reserve and a conservative tensor-parallel efficiency haircut
//! ([`FitConfig::tensor_parallel_efficiency`]) — sharding a model across GPUs
//! never yields 100% of aggregate VRAM. A single-GPU node is unaffected.
//! Cross-node (distributed) fitting is out of scope.
//!
//! The aggregate check assumes weights shard evenly across the node's cards; it
//! does not gate on per-card capacity, so a single unshardable tensor larger
//! than one card's VRAM is not caught here. The haircut is conservative
//! (multi-GPU is reported with less capacity, never more).

use amicu_core::diagnostic::{Diagnostic, DiagnosticCode, Severity, catalog};
use amicu_core::resource::{AcceleratorKind, GgufVariant, HardwareProfile, Quantization};
use amicu_core::units::ByteSize;

/// Approximate bytes per weight per quantization.
///
/// Basis: GGUF effective bits-per-weight, divided by 8. F32=4B, F16/BF16=2B and
/// Int8/Int4 are exact; the K-quants are llama.cpp effective sizes — Q8_0 8.5
/// bpw (1.0625 B), Q6_K ≈6.56 bpw (0.82 B), Q5_K_M ≈5.44 bpw (0.68 B), Q4_K_M
/// ≈4.48 bpw (0.56 B). Approximations, not exact file sizes.
#[must_use]
pub fn bytes_per_parameter(q: Quantization) -> f64 {
    match q {
        Quantization::F32 => 4.0,
        Quantization::F16 | Quantization::Bf16 => 2.0,
        Quantization::Q8_0 => 1.0625,
        Quantization::Int8 => 1.0,
        Quantization::Q6K => 0.82,
        Quantization::Q5KM => 0.68,
        Quantization::Q4KM => 0.56,
        Quantization::Int4 => 0.5,
    }
}

/// Weights-only memory for `total_parameters` at `quantization`.
#[must_use]
pub fn weights_memory(total_parameters: u64, quantization: Quantization) -> ByteSize {
    bytes_from_f64(params_as_f64(total_parameters) * bytes_per_parameter(quantization))
}

/// KV-cache + runtime overhead as fractions of the weights figure. Wide by
/// design: real overhead depends on context length, batch and runtime, so it is
/// a band, not a point estimate.
const DEFAULT_OVERHEAD_LOW: f64 = 0.10;
const DEFAULT_OVERHEAD_HIGH: f64 = 0.30;

/// OS/other-workload memory held back from the device pool, **per device**. 2
/// GiB is a visible, deliberately conservative default; override per host. On a
/// multi-GPU node this is held back on every card.
const DEFAULT_RESERVE: ByteSize = ByteSize::from_bytes(2 * 1024 * 1024 * 1024);

/// Fraction of aggregate multi-GPU VRAM usable for weights under tensor
/// parallelism. Sharding replicates buffers and reserves per-card working
/// memory, so aggregate VRAM is never fully usable. 0.90 is a conservative
/// default; applies only when `count > 1`.
const DEFAULT_TENSOR_PARALLEL_EFFICIENCY: f64 = 0.90;

/// Tunables for a fit check: the per-device memory reserve, the overhead band,
/// and the multi-GPU tensor-parallel efficiency.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct FitConfig {
    /// Memory held back for the OS and other workloads, subtracted from the
    /// device pool before evaluating fit. Applied per device (see
    /// [`DEFAULT_RESERVE`]).
    pub reserve: ByteSize,
    /// Low bound of the overhead band, as a fraction of weights.
    pub overhead_low: f64,
    /// High bound of the overhead band, as a fraction of weights.
    pub overhead_high: f64,
    /// Fraction of aggregate VRAM usable for weights when sharding across more
    /// than one accelerator (tensor parallelism). Clamped to `[0, 1]`; ignored
    /// for single-device pools.
    pub tensor_parallel_efficiency: f64,
}

impl Default for FitConfig {
    fn default() -> Self {
        Self {
            reserve: DEFAULT_RESERVE,
            overhead_low: DEFAULT_OVERHEAD_LOW,
            overhead_high: DEFAULT_OVERHEAD_HIGH,
            tensor_parallel_efficiency: DEFAULT_TENSOR_PARALLEL_EFFICIENCY,
        }
    }
}

/// KV-cache + runtime overhead estimate, as an uncertainty band (`low <= high`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverheadRange {
    /// Low estimate.
    pub low: ByteSize,
    /// High estimate.
    pub high: ByteSize,
}

/// The device memory pool a fit is evaluated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPool {
    /// Discrete accelerator VRAM.
    Vram,
    /// Unified memory shared by CPU and accelerator (Apple Silicon).
    Unified,
    /// Plain system memory (CPU-only or no accelerator).
    System,
}

impl MemoryPool {
    /// Diagnostic code raised when weights exceed this pool.
    const fn diagnostic_code(self) -> DiagnosticCode {
        match self {
            MemoryPool::Vram => catalog::MEM_VRAM_EXCEEDED,
            MemoryPool::Unified | MemoryPool::System => catalog::MEM_ARTIFACT_OVERSIZE,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            MemoryPool::Vram => "VRAM",
            MemoryPool::Unified => "unified memory",
            MemoryPool::System => "system memory",
        }
    }
}

/// Structured outcome of a weight-fit check.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FitReport {
    /// Which memory pool was evaluated.
    pub pool: MemoryPool,
    /// Total memory in the device pool. For a multi-GPU node this is the
    /// aggregate across all `accelerator_count` cards.
    pub device_memory: ByteSize,
    /// Number of accelerators aggregated (1 for a single GPU, non-GPU, or no
    /// accelerator).
    pub accelerator_count: u32,
    /// Reserve held back for the OS/other workloads, totalled across devices
    /// (per-device reserve × `accelerator_count`).
    pub reserve: ByteSize,
    /// Memory available after the reserve (`device_memory - reserve`).
    pub available: ByteSize,
    /// Weights-only memory (the certain figure).
    pub weights: ByteSize,
    /// KV-cache + runtime overhead as an uncertainty band, separate from weights.
    pub overhead: OverheadRange,
    /// Available minus weights, saturating at zero.
    pub headroom: ByteSize,
    /// Whether weights fit within `available`.
    pub fits: bool,
    /// Diagnostic when weights do not fit; `None` otherwise.
    pub diagnostic: Option<Diagnostic>,
}

/// Check whether `total_parameters` at `quantization` fit `hw` under `cfg`.
#[must_use]
pub fn check_fit(
    total_parameters: u64,
    quantization: Quantization,
    hw: &HardwareProfile,
    cfg: &FitConfig,
) -> FitReport {
    evaluate(weights_memory(total_parameters, quantization), hw, cfg)
}

/// As [`check_fit`], but honoring a variant's declared `size` when present and
/// falling back to a computed size from its quantization otherwise.
#[must_use]
pub fn check_variant_fit(
    total_parameters: u64,
    variant: &GgufVariant,
    hw: &HardwareProfile,
    cfg: &FitConfig,
) -> FitReport {
    let weights = variant
        .size
        .unwrap_or_else(|| weights_memory(total_parameters, variant.quantization));
    evaluate(weights, hw, cfg)
}

fn evaluate(weights: ByteSize, hw: &HardwareProfile, cfg: &FitConfig) -> FitReport {
    let (pool, per_device, count) = device_pool(hw);

    // Aggregate across devices. A single-device pool (count == 1) is byte-for-byte
    // unchanged; a multi-GPU pool sums per-device VRAM, then applies a
    // tensor-parallel efficiency haircut because sharding replicates buffers and
    // reserves per-card working memory, so aggregate VRAM is never fully usable.
    let device_memory = ByteSize::from_bytes(per_device.bytes().saturating_mul(u64::from(count)));
    let usable = if count > 1 {
        let eff = cfg.tensor_parallel_efficiency.clamp(0.0, 1.0);
        bytes_from_f64(params_as_f64(device_memory.bytes()) * eff).bytes()
    } else {
        device_memory.bytes()
    };
    // Reserve is per device: every card holds back OS/driver/context memory.
    let reserve = ByteSize::from_bytes(cfg.reserve.bytes().saturating_mul(u64::from(count)));
    let available = ByteSize::from_bytes(usable.saturating_sub(reserve.bytes()));
    let fits = weights.bytes() <= available.bytes();
    let headroom = ByteSize::from_bytes(available.bytes().saturating_sub(weights.bytes()));

    let low = cfg.overhead_low.min(cfg.overhead_high);
    let high = cfg.overhead_low.max(cfg.overhead_high);
    let weights_f = params_as_f64(weights.bytes());
    let overhead = OverheadRange {
        low: bytes_from_f64(weights_f * low),
        high: bytes_from_f64(weights_f * high),
    };

    let diagnostic = (!fits).then(|| {
        let across = if count > 1 {
            format!(
                " across {count} × {model} accelerators",
                model = hw.accelerator.as_ref().map_or("", |a| a.model.as_str()),
            )
        } else {
            String::new()
        };
        Diagnostic::new(
            pool.diagnostic_code(),
            Severity::Error,
            format!(
                "model weights need {weights} but only {available} of {device_memory} {pool}{across} \
                 is available after a {reserve} reserve",
                pool = pool.label(),
            ),
        )
    });

    FitReport {
        pool,
        device_memory,
        accelerator_count: count,
        reserve,
        available,
        weights,
        overhead,
        headroom,
        fits,
        diagnostic,
    }
}

/// Pick the memory pool, per-device size, and device count to evaluate against.
/// Apple Silicon uses unified system memory; a discrete accelerator (GPU/TPU)
/// uses its own VRAM aggregated across `count` cards; CPU-only or no accelerator
/// uses system memory. Count is 1 for every non-multi-GPU pool.
fn device_pool(hw: &HardwareProfile) -> (MemoryPool, ByteSize, u32) {
    match &hw.accelerator {
        Some(acc) => match acc.kind {
            AcceleratorKind::AppleSilicon => (MemoryPool::Unified, hw.memory, 1),
            AcceleratorKind::Gpu | AcceleratorKind::Tpu => {
                (MemoryPool::Vram, acc.memory, acc.count.max(1))
            }
            AcceleratorKind::Cpu => (MemoryPool::System, hw.memory, 1),
        },
        None => (MemoryPool::System, hw.memory, 1),
    }
}

#[allow(clippy::cast_precision_loss)]
fn params_as_f64(p: u64) -> f64 {
    p as f64
}

// Non-negative, finite inputs by construction; clamp guards the cast.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn bytes_from_f64(v: f64) -> ByteSize {
    let clamped = v.round().clamp(0.0, u64::MAX as f64);
    ByteSize::from_bytes(clamped as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    use amicu_core::id::HardwareProfileId;
    use amicu_core::resource::Accelerator;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn apple_silicon(mem_gib: u64) -> HardwareProfile {
        HardwareProfile {
            id: HardwareProfileId::new("m3_max"),
            provider: "on_prem".into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::AppleSilicon,
                model: "m3_max".into(),
                count: 1,
                memory: ByteSize::from_bytes(mem_gib * GIB),
            }),
            cpu: None,
            memory: ByteSize::from_bytes(mem_gib * GIB),
            interconnect: None,
            price: None,
        }
    }

    fn gpu(vram_gib: u64) -> HardwareProfile {
        multi_gpu(vram_gib, 1)
    }

    fn multi_gpu(vram_gib_each: u64, count: u32) -> HardwareProfile {
        HardwareProfile {
            id: HardwareProfileId::new("h100"),
            provider: "aws".into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "h100".into(),
                count,
                memory: ByteSize::from_bytes(vram_gib_each * GIB),
            }),
            cpu: None,
            memory: ByteSize::from_bytes(256 * GIB),
            interconnect: None,
            price: None,
        }
    }

    #[test]
    fn bytes_per_parameter_table() {
        assert!((bytes_per_parameter(Quantization::F32) - 4.0).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::F16) - 2.0).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Bf16) - 2.0).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Q8_0) - 1.0625).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Int8) - 1.0).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Q6K) - 0.82).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Q5KM) - 0.68).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Q4KM) - 0.56).abs() < f64::EPSILON);
        assert!((bytes_per_parameter(Quantization::Int4) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn weights_memory_for_7b_q4km_is_near_4gib() {
        let w = weights_memory(7_000_000_000, Quantization::Q4KM);
        // 7e9 * 0.56 = 3.92 GB ≈ 3.65 GiB; assert a loose GiB-scale band.
        assert!(w.bytes() > 3 * GIB && w.bytes() < 4 * GIB, "got {w}");
    }

    #[test]
    fn fits_on_unified_memory_with_reserve() {
        // 7B @ Q4_K_M (~3.65 GiB) on a 16 GiB Mac, 2 GiB reserve -> fits.
        let report = check_fit(
            7_000_000_000,
            Quantization::Q4KM,
            &apple_silicon(16),
            &FitConfig::default(),
        );
        assert_eq!(report.pool, MemoryPool::Unified);
        assert!(report.fits);
        assert_eq!(report.available, ByteSize::from_bytes(14 * GIB));
        assert!(report.headroom.bytes() > 0);
        assert!(report.diagnostic.is_none());
    }

    #[test]
    fn oversize_on_unified_memory_yields_artifact_oversize() {
        // 70B @ F16 (~140 GiB) on an 8 GiB Mac -> does not fit.
        let report = check_fit(
            70_000_000_000,
            Quantization::F16,
            &apple_silicon(8),
            &FitConfig::default(),
        );
        assert!(!report.fits);
        assert_eq!(report.headroom, ByteSize::from_bytes(0));
        let diag = report.diagnostic.expect("diagnostic");
        assert_eq!(diag.code, catalog::MEM_ARTIFACT_OVERSIZE);
        assert_eq!(diag.severity, Severity::Error);
    }

    #[test]
    fn oversize_on_gpu_yields_vram_exceeded() {
        // 70B @ Q8_0 (~74 GiB) on a 24 GiB GPU -> VRAM exceeded.
        let report = check_fit(
            70_000_000_000,
            Quantization::Q8_0,
            &gpu(24),
            &FitConfig::default(),
        );
        assert_eq!(report.pool, MemoryPool::Vram);
        assert!(!report.fits);
        // Evaluated against VRAM, not the 256 GiB system memory.
        assert_eq!(report.device_memory, ByteSize::from_bytes(24 * GIB));
        let diag = report.diagnostic.expect("diagnostic");
        assert_eq!(diag.code, catalog::MEM_VRAM_EXCEEDED);
    }

    #[test]
    fn fits_on_gpu_vram() {
        // 7B @ Q4_K_M on an 80 GiB GPU -> fits with room.
        let report = check_fit(
            7_000_000_000,
            Quantization::Q4KM,
            &gpu(80),
            &FitConfig::default(),
        );
        assert!(report.fits);
        assert!(report.diagnostic.is_none());
    }

    #[test]
    fn overhead_band_is_present_and_ordered() {
        let report = check_fit(
            7_000_000_000,
            Quantization::Q4KM,
            &apple_silicon(16),
            &FitConfig::default(),
        );
        assert!(report.overhead.low <= report.overhead.high);
        assert!(report.overhead.low.bytes() > 0);
        // Separate from weights: overhead high is a fraction of weights.
        assert!(report.overhead.high.bytes() < report.weights.bytes());
    }

    #[test]
    fn variant_size_overrides_computed_weights() {
        let variant = GgufVariant {
            quantization: Quantization::Q4KM,
            file: "m.Q4_K_M.gguf".into(),
            size: Some(ByteSize::from_bytes(4 * GIB)),
        };
        let report = check_variant_fit(
            7_000_000_000,
            &variant,
            &apple_silicon(16),
            &FitConfig::default(),
        );
        assert_eq!(report.weights, ByteSize::from_bytes(4 * GIB));
        assert!(report.fits);
    }

    #[test]
    fn single_gpu_behavior_is_unchanged() {
        // count == 1: no aggregation, no efficiency haircut, single reserve.
        let report = check_fit(
            1_000_000,
            Quantization::F16,
            &gpu(80),
            &FitConfig::default(),
        );
        assert_eq!(report.accelerator_count, 1);
        assert_eq!(report.device_memory, ByteSize::from_bytes(80 * GIB));
        assert_eq!(report.reserve, ByteSize::from_bytes(2 * GIB));
        assert_eq!(report.available, ByteSize::from_bytes(78 * GIB));
    }

    #[test]
    fn aggregate_vram_across_gpus_lets_large_model_fit() {
        // 70B @ F16 (~130 GiB) does not fit one 80 GiB H100 but fits 8×H100
        // (aggregate 640 GiB, ~90% usable minus per-device reserve).
        let one = check_fit(
            70_000_000_000,
            Quantization::F16,
            &gpu(80),
            &FitConfig::default(),
        );
        assert!(!one.fits);
        let eight = check_fit(
            70_000_000_000,
            Quantization::F16,
            &multi_gpu(80, 8),
            &FitConfig::default(),
        );
        assert!(eight.fits);
        assert_eq!(eight.accelerator_count, 8);
        assert_eq!(eight.device_memory, ByteSize::from_bytes(640 * GIB));
    }

    #[test]
    fn gpu_count_changes_the_fit_outcome() {
        // 70B @ Q8_0 (~69 GiB): does not fit 1×24 GiB, fits 4×24 GiB aggregate.
        let one = check_fit(
            70_000_000_000,
            Quantization::Q8_0,
            &gpu(24),
            &FitConfig::default(),
        );
        assert!(!one.fits);
        let four = check_fit(
            70_000_000_000,
            Quantization::Q8_0,
            &multi_gpu(24, 4),
            &FitConfig::default(),
        );
        assert!(four.fits);
    }

    #[test]
    fn tensor_parallel_haircut_and_per_device_reserve_apply_for_multi_gpu() {
        // 2×80 GiB: aggregate 160, 90% usable = 144, minus 2×2 GiB reserve = 140.
        let two = check_fit(
            1_000_000,
            Quantization::F16,
            &multi_gpu(80, 2),
            &FitConfig::default(),
        );
        assert_eq!(two.device_memory, ByteSize::from_bytes(160 * GIB));
        assert_eq!(two.reserve, ByteSize::from_bytes(4 * GIB));
        assert_eq!(two.available, ByteSize::from_bytes(140 * GIB));
    }

    #[test]
    fn tensor_parallel_efficiency_is_configurable() {
        let cfg = FitConfig {
            tensor_parallel_efficiency: 0.5,
            ..FitConfig::default()
        };
        // 2×80 GiB: aggregate 160, 50% usable = 80, minus 4 GiB reserve = 76.
        let two = check_fit(1_000_000, Quantization::F16, &multi_gpu(80, 2), &cfg);
        assert_eq!(two.available, ByteSize::from_bytes(76 * GIB));
    }

    #[test]
    fn zero_count_is_guarded_to_one_device() {
        // A malformed count of 0 must not zero the pool; it is treated as 1.
        let report = check_fit(
            1_000_000,
            Quantization::F16,
            &multi_gpu(80, 0),
            &FitConfig::default(),
        );
        assert_eq!(report.accelerator_count, 1);
        assert_eq!(report.device_memory, ByteSize::from_bytes(80 * GIB));
        assert_eq!(report.available, ByteSize::from_bytes(78 * GIB));
    }

    #[test]
    fn multi_tpu_aggregates_the_same_way() {
        let mut hw = multi_gpu(32, 4);
        if let Some(acc) = hw.accelerator.as_mut() {
            acc.kind = AcceleratorKind::Tpu;
            acc.model = "v5e".into();
        }
        let report = check_fit(1_000_000, Quantization::F16, &hw, &FitConfig::default());
        assert_eq!(report.pool, MemoryPool::Vram);
        assert_eq!(report.accelerator_count, 4);
        // 4×32 = 128 GiB aggregate, 90% usable = 115.2, minus 4×2 reserve.
        assert_eq!(report.device_memory, ByteSize::from_bytes(128 * GIB));
        assert_eq!(report.reserve, ByteSize::from_bytes(8 * GIB));
    }

    #[test]
    fn variant_without_size_falls_back_to_computed() {
        let variant = GgufVariant {
            quantization: Quantization::Q4KM,
            file: "m.Q4_K_M.gguf".into(),
            size: None,
        };
        let report = check_variant_fit(
            7_000_000_000,
            &variant,
            &apple_silicon(16),
            &FitConfig::default(),
        );
        assert_eq!(
            report.weights,
            weights_memory(7_000_000_000, Quantization::Q4KM)
        );
    }
}
