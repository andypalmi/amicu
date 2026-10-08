//! GGUF variant and quantization resolution over a model's declared variants.
//!
//! A model's artifact is only GGUF-quantizable when
//! [`ArtifactFormat::Gguf`](amicu_core::resource::ArtifactFormat::Gguf); other
//! formats surface as [`GgufError::NotGguf`] rather than panicking.

use amicu_core::resource::{ArtifactFormat, GgufVariant, Model, Quantization};

/// Preference order for [`GgufResolver::recommended`]: highest-quality quant
/// that keeps a sane size/quality balance for local serving, then coarser
/// fallbacks. Q4_K_M leads as the common default.
const RECOMMENDED_ORDER: &[Quantization] = &[
    Quantization::Q4KM,
    Quantization::Q5KM,
    Quantization::Q6K,
    Quantization::Q8_0,
    Quantization::Int8,
    Quantization::Int4,
    Quantization::F16,
    Quantization::Bf16,
    Quantization::F32,
];

/// Failure resolving GGUF variants from a model.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GgufError {
    /// The model's artifact is not a GGUF format.
    #[error("model artifact is {0:?}, not GGUF")]
    NotGguf(ArtifactFormat),
    /// No variant matches the requested quantization.
    #[error("no GGUF variant for quantization {0:?}")]
    QuantizationUnavailable(Quantization),
}

/// Resolves GGUF variants and quantizations from a model's declared variants.
#[derive(Debug, Clone, Copy)]
pub struct GgufResolver<'a> {
    variants: &'a [GgufVariant],
}

impl<'a> GgufResolver<'a> {
    /// Build a resolver over a model's GGUF variants.
    ///
    /// # Errors
    /// [`GgufError::NotGguf`] if the model's artifact is not GGUF.
    pub fn new(model: &'a Model) -> Result<Self, GgufError> {
        if model.artifact.format != ArtifactFormat::Gguf {
            return Err(GgufError::NotGguf(model.artifact.format));
        }
        Ok(Self {
            variants: &model.variants,
        })
    }

    /// Build a resolver directly over a variant slice (may be empty).
    #[must_use]
    pub fn from_variants(variants: &'a [GgufVariant]) -> Self {
        Self { variants }
    }

    /// All available variants, in declaration order.
    #[must_use]
    pub fn variants(&self) -> &'a [GgufVariant] {
        self.variants
    }

    /// Available quantizations, in declaration order.
    #[must_use]
    pub fn quantizations(&self) -> Vec<Quantization> {
        self.variants.iter().map(|v| v.quantization).collect()
    }

    /// Whether any variant is available.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    /// Look up a variant by quantization.
    #[must_use]
    pub fn by_quantization(&self, q: Quantization) -> Option<&'a GgufVariant> {
        self.variants.iter().find(|v| v.quantization == q)
    }

    /// Look up a variant by quantization, erroring when unavailable.
    ///
    /// # Errors
    /// [`GgufError::QuantizationUnavailable`] when no variant matches.
    pub fn require(&self, q: Quantization) -> Result<&'a GgufVariant, GgufError> {
        self.by_quantization(q)
            .ok_or(GgufError::QuantizationUnavailable(q))
    }

    /// The recommended default variant: the first quantization in
    /// [`RECOMMENDED_ORDER`] that is available, else the first declared variant.
    /// `None` only when no variants exist.
    #[must_use]
    pub fn recommended(&self) -> Option<&'a GgufVariant> {
        RECOMMENDED_ORDER
            .iter()
            .find_map(|&q| self.by_quantization(q))
            .or_else(|| self.variants.first())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use amicu_core::id::ModelId;
    use amicu_core::resource::{Artifact, Modality, Model, ModelSource, SourceRegistry};
    use amicu_core::units::ByteSize;

    fn variant(q: Quantization, file: &str, size: Option<u64>) -> GgufVariant {
        GgufVariant {
            quantization: q,
            file: file.into(),
            size: size.map(ByteSize::from_bytes),
            sha256: None,
        }
    }

    fn model(format: ArtifactFormat, variants: Vec<GgufVariant>) -> Model {
        Model {
            id: ModelId::new("tiny_llama"),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "org/tiny-llama".into(),
            },
            architecture: "llama".into(),
            modalities: vec![Modality::Text],
            artifact: Artifact {
                format,
                quantization: None,
            },
            context_length: Some(4096),
            variants,
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
        }
    }

    fn gguf_model() -> Model {
        model(
            ArtifactFormat::Gguf,
            vec![
                variant(Quantization::Q8_0, "m.Q8_0.gguf", Some(120)),
                variant(Quantization::Q4KM, "m.Q4_K_M.gguf", Some(64)),
                variant(Quantization::Q6K, "m.Q6_K.gguf", Some(96)),
            ],
        )
    }

    #[test]
    fn resolves_variants_and_quantizations() {
        let m = gguf_model();
        let r = GgufResolver::new(&m).expect("gguf");
        assert_eq!(r.variants().len(), 3);
        assert_eq!(
            r.quantizations(),
            vec![Quantization::Q8_0, Quantization::Q4KM, Quantization::Q6K]
        );
    }

    #[test]
    fn lookup_by_quantization() {
        let m = gguf_model();
        let r = GgufResolver::new(&m).expect("gguf");
        let v = r.by_quantization(Quantization::Q4KM).expect("present");
        assert_eq!(v.file, "m.Q4_K_M.gguf");
        assert!(r.by_quantization(Quantization::F32).is_none());
        assert_eq!(
            r.require(Quantization::F32).unwrap_err(),
            GgufError::QuantizationUnavailable(Quantization::F32)
        );
    }

    #[test]
    fn recommended_prefers_q4km() {
        let m = gguf_model();
        let r = GgufResolver::new(&m).expect("gguf");
        assert_eq!(
            r.recommended().expect("some").quantization,
            Quantization::Q4KM
        );
    }

    #[test]
    fn recommended_falls_back_to_first_when_no_preferred() {
        let m = model(
            ArtifactFormat::Gguf,
            vec![variant(Quantization::F32, "m.F32.gguf", None)],
        );
        let r = GgufResolver::new(&m).expect("gguf");
        assert_eq!(
            r.recommended().expect("some").quantization,
            Quantization::F32
        );
    }

    #[test]
    fn non_gguf_artifact_errors() {
        let m = model(ArtifactFormat::Safetensors, vec![]);
        assert_eq!(
            GgufResolver::new(&m).unwrap_err(),
            GgufError::NotGguf(ArtifactFormat::Safetensors)
        );
    }

    #[test]
    fn empty_gguf_variants_resolve_but_recommend_none() {
        let m = model(ArtifactFormat::Gguf, vec![]);
        let r = GgufResolver::new(&m).expect("gguf");
        assert!(r.is_empty());
        assert!(r.recommended().is_none());
        assert_eq!(r.quantizations(), Vec::new());
    }
}
