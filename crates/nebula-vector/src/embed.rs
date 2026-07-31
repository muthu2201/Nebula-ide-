//! The embedding port, and the embedder that ships in-process.
//!
//! ## Why there is a port here at all
//!
//! Nebula's privacy guarantee is that nothing is sent anywhere the user did not
//! ask for. That rules out a hosted embedding API for the local index, so the
//! embedder must run on the user's machine. Two things then run through the
//! same trait:
//!
//! * [`HashingEmbedder`] — compiled in, no model file, no download, works
//!   offline on first launch.
//! * A neural embedder backed by a model downloaded on demand — the blueprint's
//!   `fastembed`-class path. It is loaded through [`Embedder`] so the index, the
//!   retrieval code and the tests never know which one is behind it.
//!
//! ## What the hashing embedder actually does
//!
//! It is the classic *hashing trick*: split the text into overlapping character
//! trigrams and identifier tokens, hash each into one of `dim` buckets with a
//! sign, accumulate, and L2-normalise. That yields a genuine fixed-dimensional
//! vector whose cosine similarity tracks lexical overlap — good enough to find
//! "the other file that talks about `RetryPolicy`", which is the majority of
//! what code retrieval needs.
//!
//! It is **not** a semantic model: it will not connect `car` to `automobile`.
//! Where that matters, install the neural embedder. This limit is documented
//! rather than hidden because pretending otherwise would make retrieval quality
//! impossible to reason about.

use crate::Result;

/// Turns text into vectors.
///
/// Implementations must be deterministic — the same text yields the same vector
/// — or an index built in one session stops matching queries made in the next.
pub trait Embedder: Send + Sync {
    /// Dimensionality of the vectors produced.
    fn dim(&self) -> usize;

    /// A short identifier for the embedder, stored alongside the index.
    ///
    /// Vectors from different embedders are not comparable, so an index records
    /// which one built it and refuses to answer queries from another.
    fn id(&self) -> &str;

    /// Embed one piece of text.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed several pieces of text.
    ///
    /// The default implementation is a loop; a neural embedder overrides this to
    /// batch through the model, which is where nearly all of its speed comes
    /// from.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// A feature-hashing embedder over character trigrams and identifier tokens.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: usize,
    id: String,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(384)
    }
}

impl HashingEmbedder {
    /// An embedder producing `dim`-dimensional vectors.
    ///
    /// 384 matches the dimensionality of the small sentence-transformer models,
    /// so swapping in a neural embedder later does not require re-provisioning
    /// the index dimensionality.
    pub fn new(dim: usize) -> Self {
        let dim = dim.max(16);
        Self { dim, id: format!("hashing-trigram-v1-{dim}") }
    }

    /// Split text into the features that get hashed.
    ///
    /// Two feature families, because they capture different things:
    ///
    /// * **identifier tokens**, further split on `snake_case` and `camelCase`
    ///   boundaries, so `RetryPolicy` and `retry_policy` share the features
    ///   `retry` and `policy`;
    /// * **character trigrams** over each token, which keep partial matches and
    ///   typos close together.
    fn features(text: &str) -> Vec<String> {
        let mut features = Vec::new();

        for raw_token in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if raw_token.is_empty() {
                continue;
            }
            for word in split_identifier(raw_token) {
                if word.len() < 2 {
                    continue;
                }
                let lower = word.to_lowercase();
                features.push(format!("w:{lower}"));

                // Trigrams, padded so short words still produce features.
                let padded = format!("^{lower}$");
                let chars: Vec<char> = padded.chars().collect();
                for window in chars.windows(3) {
                    features.push(format!("t:{}", window.iter().collect::<String>()));
                }
            }
        }
        features
    }

    /// FNV-1a, chosen for being fast, allocation-free and stable across runs
    /// and platforms — a hash that varied by process would invalidate every
    /// saved index.
    fn hash(feature: &str) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        for byte in feature.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut vector = vec![0.0f32; self.dim];

        for feature in Self::features(text) {
            let hash = Self::hash(&feature);
            let bucket = (hash % self.dim as u64) as usize;
            // A sign bit drawn from a different part of the hash keeps colliding
            // features from systematically reinforcing each other.
            let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
            vector[bucket] += sign;
        }

        // Sublinear scaling: a token appearing 50 times should not swamp one
        // appearing 5 times. This is the same intuition as TF-IDF's log term
        // frequency.
        for value in &mut vector {
            *value = value.signum() * (1.0 + value.abs()).ln();
        }

        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
        Ok(vector)
    }
}

/// Split an identifier on `snake_case`, `kebab-case` and `camelCase` boundaries.
///
/// `HTTPServer` splits to `HTTP` + `Server`, not `H` + `T` + `T` + `P` + `Server`
/// — a run of capitals is one word until a capital is followed by a lowercase.
fn split_identifier(identifier: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = identifier.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if c.is_uppercase() && !current.is_empty() {
            let prev_lower = chars[i - 1].is_lowercase() || chars[i - 1].is_numeric();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev_lower || next_lower {
                words.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn embeddings_are_deterministic() {
        let embedder = HashingEmbedder::new(128);
        let first = embedder.embed("fn compute_retry_policy() {}").unwrap();
        let second = embedder.embed("fn compute_retry_policy() {}").unwrap();
        assert_eq!(first, second, "an index built today must match queries made tomorrow");
    }

    #[test]
    fn embeddings_have_the_configured_dimension_and_unit_norm() {
        let embedder = HashingEmbedder::new(256);
        let vector = embedder.embed("some source code here").unwrap();
        assert_eq!(vector.len(), 256);
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn related_code_scores_higher_than_unrelated_code() {
        let embedder = HashingEmbedder::new(384);
        let query = embedder.embed("struct RetryPolicy { max_attempts: u32 }").unwrap();
        let related = embedder.embed("impl RetryPolicy { fn max_attempts(&self) -> u32 }").unwrap();
        let unrelated = embedder.embed("fn render_glyph_atlas(texture: &Texture) {}").unwrap();

        assert!(
            cosine(&query, &related) > cosine(&query, &unrelated),
            "related {:.3} should beat unrelated {:.3}",
            cosine(&query, &related),
            cosine(&query, &unrelated)
        );
    }

    #[test]
    fn naming_conventions_are_bridged() {
        let embedder = HashingEmbedder::new(384);
        let snake = embedder.embed("retry_policy").unwrap();
        let camel = embedder.embed("retryPolicy").unwrap();
        let pascal = embedder.embed("RetryPolicy").unwrap();
        let other = embedder.embed("glyph_atlas").unwrap();

        assert!(cosine(&snake, &camel) > 0.9, "snake_case and camelCase must align");
        assert!(cosine(&snake, &pascal) > 0.9, "snake_case and PascalCase must align");
        assert!(cosine(&snake, &other) < cosine(&snake, &camel));
    }

    #[test]
    fn empty_text_produces_a_zero_vector_not_a_nan() {
        let embedder = HashingEmbedder::new(64);
        let vector = embedder.embed("").unwrap();
        assert_eq!(vector.len(), 64);
        assert!(vector.iter().all(|v| v.is_finite()));
        assert!(vector.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn unicode_text_is_handled_without_panicking() {
        let embedder = HashingEmbedder::new(64);
        let vector = embedder.embed("函数 héllo 🌌 переменная").unwrap();
        assert!(vector.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn batch_matches_individual_embedding() {
        let embedder = HashingEmbedder::new(128);
        let texts = ["first text", "second text", "third text"];
        let batch = embedder.embed_batch(&texts).unwrap();
        for (i, text) in texts.iter().enumerate() {
            assert_eq!(batch[i], embedder.embed(text).unwrap());
        }
    }

    #[test]
    fn identifier_splitting_handles_the_awkward_cases() {
        assert_eq!(split_identifier("retry_policy"), vec!["retry", "policy"]);
        assert_eq!(split_identifier("retryPolicy"), vec!["retry", "Policy"]);
        assert_eq!(split_identifier("RetryPolicy"), vec!["Retry", "Policy"]);
        assert_eq!(split_identifier("kebab-case"), vec!["kebab", "case"]);
        // A run of capitals is one word until a capital starts a new word.
        assert_eq!(split_identifier("HTTPServer"), vec!["HTTP", "Server"]);
        assert_eq!(split_identifier("parseHTTPResponse"), vec!["parse", "HTTP", "Response"]);
        assert_eq!(split_identifier("simple"), vec!["simple"]);
        assert!(split_identifier("").is_empty());
    }

    #[test]
    fn the_embedder_id_records_its_dimension() {
        assert_eq!(HashingEmbedder::new(384).id(), "hashing-trigram-v1-384");
        assert_ne!(
            HashingEmbedder::new(128).id(),
            HashingEmbedder::new(384).id(),
            "indexes built at different dimensions must not be confused for each other"
        );
    }

    #[test]
    fn embedders_are_usable_as_trait_objects() {
        // The index and the retrieval code only ever see `dyn Embedder`.
        let embedder: Box<dyn Embedder> = Box::new(HashingEmbedder::new(64));
        assert_eq!(embedder.dim(), 64);
        assert_eq!(embedder.embed("text").unwrap().len(), 64);
    }

    #[test]
    fn end_to_end_retrieval_over_an_hnsw_index() {
        use crate::{Hnsw, HnswConfig};

        let embedder = HashingEmbedder::new(384);
        let corpus = [
            "pub struct RetryPolicy { pub max_attempts: u32, pub backoff: Duration }",
            "impl RetryPolicy { pub fn should_retry(&self, attempt: u32) -> bool { } }",
            "pub fn render_glyph_atlas(device: &Device, glyphs: &[Glyph]) -> Texture { }",
            "pub struct GlyphAtlas { texture: Texture, allocator: Allocator }",
            "fn parse_toml_config(source: &str) -> Result<Config, ConfigError> { }",
        ];

        let mut index = Hnsw::new(HnswConfig::new(embedder.dim()));
        for (i, text) in corpus.iter().enumerate() {
            index.insert(i as u64, &embedder.embed(text).unwrap()).unwrap();
        }

        let query = embedder.embed("retry attempts with exponential backoff").unwrap();
        let results = index.search(&query, 2).unwrap();
        assert!(
            results.iter().any(|r| r.id == 0 || r.id == 1),
            "a retry query should surface the retry code, got {results:?}"
        );

        let query = embedder.embed("glyph texture atlas rendering").unwrap();
        let results = index.search(&query, 2).unwrap();
        assert!(
            results.iter().any(|r| r.id == 2 || r.id == 3),
            "a rendering query should surface the rendering code, got {results:?}"
        );
    }
}
