//! Hierarchical Navigable Small World index.
//!
//! The structure is a stack of proximity graphs. Layer 0 holds every vector;
//! each higher layer holds an exponentially thinning sample. A search enters at
//! the top, greedily walks to the local minimum of that sparse layer, drops
//! down, and repeats — so the early layers cover distance quickly and layer 0
//! does the fine-grained work.
//!
//! Implementation follows Malkov & Yashunin (2016), including the neighbour
//! selection heuristic from Algorithm 4, which is what keeps the graph
//! navigable instead of letting hubs form.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use serde::{Deserialize, Serialize};

use crate::{Result, VectorError};

/// How distance between two vectors is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    /// Squared euclidean distance. Monotonic with euclidean distance, and skips
    /// a square root per comparison.
    L2,
    /// Cosine distance, `1 - cos(a, b)`.
    ///
    /// Vectors are normalised on insert, so this reduces to `1 - dot(a, b)`.
    /// This is the right choice for text embeddings, where magnitude carries no
    /// meaning.
    Cosine,
}

impl Metric {
    /// Distance between two vectors of equal length.
    #[inline]
    pub fn distance(&self, a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len());
        match self {
            Metric::L2 => {
                let mut sum = 0.0f32;
                for (x, y) in a.iter().zip(b.iter()) {
                    let d = x - y;
                    sum += d * d;
                }
                sum
            }
            Metric::Cosine => {
                let mut dot = 0.0f32;
                for (x, y) in a.iter().zip(b.iter()) {
                    dot += x * y;
                }
                // Inputs are unit-normalised on insert and on query, so the
                // denominator is 1 and this is exact.
                1.0 - dot
            }
        }
    }

    /// Whether this metric requires unit-normalised vectors.
    #[inline]
    fn normalizes(&self) -> bool {
        matches!(self, Metric::Cosine)
    }
}

/// Index tuning parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HnswConfig {
    /// Vector dimensionality.
    pub dim: usize,
    /// Distance metric.
    pub metric: Metric,
    /// Maximum outgoing links per node on layers above 0.
    ///
    /// Higher values improve recall and cost memory and build time. 16 is the
    /// value the paper recommends for most workloads.
    pub m: usize,
    /// Size of the candidate list during construction. Higher means a better
    /// graph and a slower build.
    pub ef_construction: usize,
    /// Default size of the candidate list during search. Must be at least the
    /// requested `k`; the search raises it automatically if it is not.
    pub ef_search: usize,
    /// Seed for level assignment, so builds are reproducible.
    pub seed: u64,
}

impl HnswConfig {
    /// Defaults tuned for text embeddings of `dim` dimensions.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            metric: Metric::Cosine,
            m: 16,
            ef_construction: 200,
            ef_search: 64,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Use a different metric.
    pub fn metric(mut self, metric: Metric) -> Self {
        self.metric = metric;
        self
    }

    /// Set the connectivity parameter.
    pub fn m(mut self, m: usize) -> Self {
        self.m = m.max(2);
        self
    }

    /// Set the construction candidate list size.
    pub fn ef_construction(mut self, ef: usize) -> Self {
        self.ef_construction = ef.max(self.m);
        self
    }

    /// Set the default search candidate list size.
    pub fn ef_search(mut self, ef: usize) -> Self {
        self.ef_search = ef.max(1);
        self
    }

    /// Set the level-assignment seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Maximum links on layer 0.
    ///
    /// The paper uses `2 * m` here: layer 0 carries every node, so it needs more
    /// connectivity to stay navigable.
    #[inline]
    fn m0(&self) -> usize {
        self.m * 2
    }

    /// Level generation normalisation factor, `1 / ln(m)`.
    #[inline]
    fn level_multiplier(&self) -> f64 {
        1.0 / (self.m as f64).ln()
    }
}

/// A search result: which vector, and how far away.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Neighbor {
    /// The caller's identifier for the vector.
    pub id: u64,
    /// Distance under the index's metric. Smaller is closer.
    pub distance: f32,
}

/// A candidate in a priority queue, ordered by distance.
///
/// `f32` is not `Ord`, and distances here are guaranteed finite by the insert
/// path, so a total order via `partial_cmp` is sound.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    node: u32,
    distance: f32,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap on distance: the farthest candidate sits on top, which is
        // what the "drop the worst" step of the algorithm needs.
        self.distance.partial_cmp(&other.distance).unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A candidate ordered the other way, for the "closest first" frontier.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MinCandidate(Candidate);

impl Eq for MinCandidate {}

impl Ord for MinCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for MinCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One indexed vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    /// The caller's identifier.
    id: u64,
    /// The stored (possibly normalised) vector.
    vector: Vec<f32>,
    /// Adjacency, indexed by layer. `links[0]` is layer 0.
    links: Vec<Vec<u32>>,
    /// Tombstone. Removal from a HNSW graph without rebuilding would disconnect
    /// it, so deletes mark the node and searches skip it.
    deleted: bool,
}

/// An HNSW index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hnsw {
    config: HnswConfig,
    nodes: Vec<Node>,
    /// Index of the entry point, on the highest populated layer.
    entry: Option<u32>,
    /// Highest layer index currently populated.
    max_layer: usize,
    /// State of the deterministic level-assignment PRNG.
    rng: u64,
    /// Number of live (non-deleted) nodes.
    live: usize,
}

impl Hnsw {
    /// An empty index.
    pub fn new(config: HnswConfig) -> Self {
        Self {
            rng: config.seed | 1,
            config,
            nodes: Vec::new(),
            entry: None,
            max_layer: 0,
            live: 0,
        }
    }

    /// The configuration in force.
    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    /// Number of live vectors.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the index holds no live vectors.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Number of slots including tombstones, which is what memory use tracks.
    pub fn capacity_used(&self) -> usize {
        self.nodes.len()
    }

    /// Insert a vector.
    ///
    /// Inserting an `id` that already exists replaces it: the old node is
    /// tombstoned and a new one is linked in. That keeps re-indexing an edited
    /// file a single call.
    pub fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()> {
        let vector = self.prepare(vector)?;

        // Replace-by-id: tombstone any previous vector with this id.
        if let Some(existing) = self.nodes.iter_mut().find(|n| n.id == id && !n.deleted) {
            existing.deleted = true;
            self.live -= 1;
        }

        let level = self.random_level();
        let new_index = self.nodes.len() as u32;
        self.nodes.push(Node {
            id,
            vector,
            links: vec![Vec::new(); level + 1],
            deleted: false,
        });
        self.live += 1;

        let Some(entry) = self.entry else {
            // First node: it becomes the entry point.
            self.entry = Some(new_index);
            self.max_layer = level;
            return Ok(());
        };

        let query = self.nodes[new_index as usize].vector.clone();
        let mut current = entry;

        // Phase 1: greedy descent through the layers above the new node's own
        // level. Nothing is linked here, we are only finding a good entry point.
        let mut layer = self.max_layer;
        while layer > level {
            current = self.greedy_search(&query, current, layer);
            if layer == 0 {
                break;
            }
            layer -= 1;
        }

        // Phase 2: from the new node's level down to 0, find neighbours and link.
        let mut entry_points = vec![current];
        for layer in (0..=level.min(self.max_layer)).rev() {
            let candidates =
                self.search_layer(&query, &entry_points, self.config.ef_construction, layer);

            let max_links = if layer == 0 { self.config.m0() } else { self.config.m };
            let selected = self.select_neighbors(&query, &candidates, max_links);

            // Link both directions.
            for &neighbor in &selected {
                self.nodes[new_index as usize].links[layer].push(neighbor);
                self.nodes[neighbor as usize].links[layer].push(new_index);

                // Adding a backlink can push a neighbour over its budget; prune
                // it back with the same heuristic so the graph stays balanced.
                if self.nodes[neighbor as usize].links[layer].len() > max_links {
                    self.prune_links(neighbor, layer, max_links);
                }
            }

            entry_points = if candidates.is_empty() {
                selected.clone()
            } else {
                candidates.iter().map(|c| c.node).collect()
            };
            if entry_points.is_empty() {
                entry_points = vec![current];
            }
        }

        if level > self.max_layer {
            self.max_layer = level;
            self.entry = Some(new_index);
        }

        Ok(())
    }

    /// Insert many vectors.
    pub fn insert_batch(&mut self, items: impl IntoIterator<Item = (u64, Vec<f32>)>) -> Result<()> {
        for (id, vector) in items {
            self.insert(id, &vector)?;
        }
        Ok(())
    }

    /// Tombstone the vector with `id`. Returns whether anything was removed.
    pub fn remove(&mut self, id: u64) -> bool {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.id == id && !n.deleted) {
            node.deleted = true;
            self.live -= 1;
            true
        } else {
            false
        }
    }

    /// Whether `id` is present and live.
    pub fn contains(&self, id: u64) -> bool {
        self.nodes.iter().any(|n| n.id == id && !n.deleted)
    }

    /// Find the `k` nearest vectors to `query`, closest first.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<Neighbor>> {
        self.search_with_ef(query, k, self.config.ef_search.max(k))
    }

    /// Search with an explicit candidate-list size, trading speed for recall.
    pub fn search_with_ef(&self, query: &[f32], k: usize, ef: usize) -> Result<Vec<Neighbor>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let query = self.prepare(query)?;
        let Some(entry) = self.entry else {
            return Ok(Vec::new());
        };
        if self.live == 0 {
            return Ok(Vec::new());
        }

        // Greedy descent to layer 1, then a proper ef-search on layer 0.
        let mut current = entry;
        let mut layer = self.max_layer;
        while layer > 0 {
            current = self.greedy_search(&query, current, layer);
            layer -= 1;
        }

        let ef = ef.max(k);
        let candidates = self.search_layer(&query, &[current], ef, 0);

        let mut results: Vec<Neighbor> = candidates
            .into_iter()
            .filter(|c| !self.nodes[c.node as usize].deleted)
            .map(|c| Neighbor { id: self.nodes[c.node as usize].id, distance: c.distance })
            .collect();

        results.sort_by(|a, b| {
            a.distance.partial_cmp(&b.distance).unwrap_or(Ordering::Equal).then(a.id.cmp(&b.id))
        });
        results.truncate(k);
        Ok(results)
    }

    /// Exhaustive search, used to measure the approximate index's recall.
    ///
    /// This is what `search` is an approximation *of*; it exists so the recall
    /// tests have ground truth, and so a caller with a tiny corpus can skip the
    /// graph entirely.
    pub fn search_exact(&self, query: &[f32], k: usize) -> Result<Vec<Neighbor>> {
        let query = self.prepare(query)?;
        let mut all: Vec<Neighbor> = self
            .nodes
            .iter()
            .filter(|n| !n.deleted)
            .map(|n| Neighbor {
                id: n.id,
                distance: self.config.metric.distance(&query, &n.vector),
            })
            .collect();
        all.sort_by(|a, b| {
            a.distance.partial_cmp(&b.distance).unwrap_or(Ordering::Equal).then(a.id.cmp(&b.id))
        });
        all.truncate(k);
        Ok(all)
    }

    /// Serialise the index to bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|e| VectorError::Serialization(e.to_string()))
    }

    /// Restore an index from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (index, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|e| VectorError::Serialization(e.to_string()))?;
        Ok(index)
    }

    /// Write the index to a file.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        std::fs::write(path, self.to_bytes()?)?;
        Ok(())
    }

    /// Read an index back from a file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Rebuild the graph, dropping tombstoned nodes.
    ///
    /// Tombstones cost search time and memory forever; a project that has been
    /// edited for a week wants this run once rather than a full re-embed.
    pub fn compact(&mut self) -> Result<()> {
        let live: Vec<(u64, Vec<f32>)> = self
            .nodes
            .iter()
            .filter(|n| !n.deleted)
            .map(|n| (n.id, n.vector.clone()))
            .collect();

        let mut rebuilt = Hnsw::new(self.config);
        for (id, vector) in live {
            // Vectors are already normalised, so this re-normalisation is a
            // no-op that keeps the code path single.
            rebuilt.insert(id, &vector)?;
        }
        *self = rebuilt;
        Ok(())
    }

    /// Validate and normalise an incoming vector.
    fn prepare(&self, vector: &[f32]) -> Result<Vec<f32>> {
        if vector.len() != self.config.dim {
            return Err(VectorError::DimensionMismatch {
                expected: self.config.dim,
                actual: vector.len(),
            });
        }
        for (index, value) in vector.iter().enumerate() {
            if !value.is_finite() {
                return Err(VectorError::NonFinite { index });
            }
        }
        if !self.config.metric.normalizes() {
            return Ok(vector.to_vec());
        }

        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 {
            // A zero vector has no direction; cosine distance is undefined, so
            // keep it as-is and let it sit at distance 1 from everything.
            return Ok(vector.to_vec());
        }
        Ok(vector.iter().map(|v| v / norm).collect())
    }

    /// Draw a level from the exponential distribution the paper specifies.
    fn random_level(&mut self) -> usize {
        // xorshift64*: small, fast, and deterministic given the seed.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let value = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);

        // Map to (0, 1], avoiding exactly 0 which would give an infinite level.
        let unit = ((value >> 11) as f64 / (1u64 << 53) as f64).max(f64::MIN_POSITIVE);
        let level = (-unit.ln() * self.config.level_multiplier()).floor() as usize;
        // Cap the level: an unlucky draw producing level 40 on a 100-node index
        // wastes a lot of empty layers.
        level.min(32)
    }

    /// Walk greedily downhill on one layer until no neighbour is closer.
    fn greedy_search(&self, query: &[f32], entry: u32, layer: usize) -> u32 {
        let mut current = entry;
        let mut current_distance = self.config.metric.distance(query, &self.nodes[current as usize].vector);

        loop {
            let mut improved = false;
            let node = &self.nodes[current as usize];
            if layer >= node.links.len() {
                break;
            }
            for &neighbor in &node.links[layer] {
                let distance =
                    self.config.metric.distance(query, &self.nodes[neighbor as usize].vector);
                if distance < current_distance {
                    current_distance = distance;
                    current = neighbor;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
        current
    }

    /// The paper's Algorithm 2: best-first search over one layer, keeping the
    /// `ef` closest results found.
    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[u32],
        ef: usize,
        layer: usize,
    ) -> Vec<Candidate> {
        let mut visited: Vec<bool> = vec![false; self.nodes.len()];
        // Frontier, closest first.
        let mut frontier: BinaryHeap<MinCandidate> = BinaryHeap::new();
        // Results, farthest first, so the worst can be evicted in O(log n).
        let mut results: BinaryHeap<Candidate> = BinaryHeap::new();

        for &entry in entry_points {
            if entry as usize >= self.nodes.len() || visited[entry as usize] {
                continue;
            }
            visited[entry as usize] = true;
            let distance = self.config.metric.distance(query, &self.nodes[entry as usize].vector);
            let candidate = Candidate { node: entry, distance };
            frontier.push(MinCandidate(candidate));
            results.push(candidate);
        }

        while let Some(MinCandidate(closest)) = frontier.pop() {
            // Everything left in the frontier is farther than our worst result,
            // so no further exploration can improve it.
            if let Some(worst) = results.peek()
                && closest.distance > worst.distance
                && results.len() >= ef
            {
                break;
            }

            let node = &self.nodes[closest.node as usize];
            if layer >= node.links.len() {
                continue;
            }
            for &neighbor in &node.links[layer] {
                let idx = neighbor as usize;
                if idx >= self.nodes.len() || visited[idx] {
                    continue;
                }
                visited[idx] = true;

                let distance = self.config.metric.distance(query, &self.nodes[idx].vector);
                let worst = results.peek().map(|c| c.distance).unwrap_or(f32::INFINITY);
                if results.len() < ef || distance < worst {
                    let candidate = Candidate { node: neighbor, distance };
                    frontier.push(MinCandidate(candidate));
                    results.push(candidate);
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        let mut out = results.into_vec();
        out.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap_or(Ordering::Equal));
        out
    }

    /// The paper's Algorithm 4: pick up to `max` neighbours, preferring ones
    /// that are closer to the query than to any already-selected neighbour.
    ///
    /// Taking the `max` nearest instead would cluster all links into one dense
    /// region and leave the graph unable to reach the rest of the space.
    fn select_neighbors(&self, query: &[f32], candidates: &[Candidate], max: usize) -> Vec<u32> {
        let mut selected: Vec<u32> = Vec::with_capacity(max);

        for candidate in candidates {
            if selected.len() >= max {
                break;
            }
            let candidate_vector = &self.nodes[candidate.node as usize].vector;

            // Keep this candidate only if it is not dominated by an existing
            // pick — i.e. it opens a direction nothing selected already covers.
            let dominated = selected.iter().any(|&chosen| {
                let chosen_vector = &self.nodes[chosen as usize].vector;
                self.config.metric.distance(candidate_vector, chosen_vector) < candidate.distance
            });
            if !dominated {
                selected.push(candidate.node);
            }
        }

        // If the heuristic was too strict, top up with the nearest remaining
        // candidates rather than leaving the node under-connected.
        if selected.len() < max {
            for candidate in candidates {
                if selected.len() >= max {
                    break;
                }
                if !selected.contains(&candidate.node) {
                    selected.push(candidate.node);
                }
            }
        }
        let _ = query;
        selected
    }

    /// Trim a node's links back to `max`, keeping the most useful ones.
    fn prune_links(&mut self, node: u32, layer: usize, max: usize) {
        let vector = self.nodes[node as usize].vector.clone();
        let links = self.nodes[node as usize].links[layer].clone();

        let mut candidates: Vec<Candidate> = links
            .iter()
            .map(|&neighbor| Candidate {
                node: neighbor,
                distance: self.config.metric.distance(&vector, &self.nodes[neighbor as usize].vector),
            })
            .collect();
        candidates.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap_or(Ordering::Equal));

        let kept = self.select_neighbors(&vector, &candidates, max);
        self.nodes[node as usize].links[layer] = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random vectors, so tests are reproducible.
    fn vectors(count: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ((v >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
        };
        (0..count).map(|_| (0..dim).map(|_| next()).collect()).collect()
    }

    fn build(count: usize, dim: usize, metric: Metric) -> (Hnsw, Vec<Vec<f32>>) {
        let config = HnswConfig::new(dim).metric(metric);
        let mut index = Hnsw::new(config);
        let data = vectors(count, dim, 42);
        for (i, v) in data.iter().enumerate() {
            index.insert(i as u64, v).unwrap();
        }
        (index, data)
    }

    #[test]
    fn empty_index_returns_no_results() {
        let index = Hnsw::new(HnswConfig::new(4));
        assert!(index.is_empty());
        assert!(index.search(&[1.0, 0.0, 0.0, 0.0], 5).unwrap().is_empty());
    }

    #[test]
    fn a_single_vector_is_its_own_nearest_neighbour() {
        let mut index = Hnsw::new(HnswConfig::new(3));
        index.insert(7, &[1.0, 2.0, 3.0]).unwrap();
        let results = index.search(&[1.0, 2.0, 3.0], 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 7);
        assert!(results[0].distance < 1e-6, "distance to itself should be ~0");
    }

    #[test]
    fn exact_matches_rank_first() {
        let (index, data) = build(500, 32, Metric::Cosine);
        for probe in [0usize, 137, 499] {
            let results = index.search(&data[probe], 5).unwrap();
            assert_eq!(
                results[0].id, probe as u64,
                "querying with an indexed vector must return it first"
            );
        }
    }

    #[test]
    fn recall_against_exhaustive_search_is_high() {
        // The headline quality property: the approximate graph must find
        // essentially the same neighbours as a brute-force scan.
        let (index, _data) = build(2_000, 64, Metric::Cosine);
        let k = 10;
        let queries = vectors(50, 64, 99);

        let mut hits = 0usize;
        let mut total = 0usize;
        for query in &queries {
            let approximate = index.search(query, k).unwrap();
            let exact = index.search_exact(query, k).unwrap();
            let exact_ids: std::collections::HashSet<u64> = exact.iter().map(|n| n.id).collect();
            hits += approximate.iter().filter(|n| exact_ids.contains(&n.id)).count();
            total += k;
        }
        let recall = hits as f64 / total as f64;
        assert!(recall > 0.95, "recall@{k} was {recall:.3}, expected > 0.95");
    }

    #[test]
    fn raising_ef_does_not_reduce_recall() {
        let (index, data) = build(1_000, 32, Metric::Cosine);
        let query = &data[0];
        let low = index.search_with_ef(query, 10, 10).unwrap();
        let high = index.search_with_ef(query, 10, 200).unwrap();
        // A larger candidate list explores more of the graph, so its worst
        // result can only be as good or better.
        assert!(
            high.last().unwrap().distance <= low.last().unwrap().distance + 1e-6,
            "ef=200 produced worse results than ef=10"
        );
    }

    #[test]
    fn results_are_sorted_by_distance() {
        let (index, data) = build(500, 16, Metric::L2);
        let results = index.search(&data[10], 20).unwrap();
        for pair in results.windows(2) {
            assert!(pair[0].distance <= pair[1].distance, "results must be closest-first");
        }
    }

    #[test]
    fn l2_metric_finds_the_geometrically_nearest_point() {
        let mut index = Hnsw::new(HnswConfig::new(2).metric(Metric::L2));
        index.insert(1, &[0.0, 0.0]).unwrap();
        index.insert(2, &[10.0, 10.0]).unwrap();
        index.insert(3, &[1.0, 1.0]).unwrap();

        let results = index.search(&[0.9, 0.9], 1).unwrap();
        assert_eq!(results[0].id, 3);
    }

    #[test]
    fn cosine_metric_ignores_magnitude() {
        let mut index = Hnsw::new(HnswConfig::new(2).metric(Metric::Cosine));
        index.insert(1, &[1.0, 0.0]).unwrap();
        index.insert(2, &[0.0, 1.0]).unwrap();

        // Same direction as vector 1, a hundred times longer.
        let results = index.search(&[100.0, 0.0], 1).unwrap();
        assert_eq!(results[0].id, 1);
        assert!(results[0].distance < 1e-6);
    }

    #[test]
    fn dimension_mismatches_are_rejected() {
        let mut index = Hnsw::new(HnswConfig::new(4));
        assert!(matches!(
            index.insert(1, &[1.0, 2.0]),
            Err(VectorError::DimensionMismatch { expected: 4, actual: 2 })
        ));
        assert!(index.search(&[1.0, 2.0], 1).is_err());
    }

    #[test]
    fn non_finite_values_are_rejected_at_the_door() {
        // A single NaN would poison every comparison it takes part in.
        let mut index = Hnsw::new(HnswConfig::new(3));
        assert!(matches!(
            index.insert(1, &[1.0, f32::NAN, 3.0]),
            Err(VectorError::NonFinite { index: 1 })
        ));
        assert!(matches!(
            index.insert(2, &[1.0, 2.0, f32::INFINITY]),
            Err(VectorError::NonFinite { index: 2 })
        ));
        assert!(index.is_empty());
    }

    #[test]
    fn a_zero_vector_is_accepted_without_producing_nan() {
        let mut index = Hnsw::new(HnswConfig::new(3).metric(Metric::Cosine));
        index.insert(1, &[0.0, 0.0, 0.0]).unwrap();
        index.insert(2, &[1.0, 0.0, 0.0]).unwrap();
        let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert!(results.iter().all(|r| r.distance.is_finite()));
    }

    #[test]
    fn removed_vectors_disappear_from_results() {
        let (mut index, data) = build(300, 16, Metric::Cosine);
        assert!(index.contains(42));
        assert!(index.remove(42));
        assert!(!index.contains(42));
        assert!(!index.remove(42), "removing twice reports nothing removed");

        let results = index.search(&data[42], 10).unwrap();
        assert!(results.iter().all(|r| r.id != 42), "a tombstoned vector must not be returned");
        assert_eq!(index.len(), 299);
    }

    #[test]
    fn reinserting_an_id_replaces_the_old_vector() {
        let mut index = Hnsw::new(HnswConfig::new(2).metric(Metric::L2));
        index.insert(1, &[0.0, 0.0]).unwrap();
        index.insert(2, &[5.0, 5.0]).unwrap();
        assert_eq!(index.len(), 2);

        index.insert(1, &[5.1, 5.1]).unwrap();
        assert_eq!(index.len(), 2, "replacing must not grow the live count");

        let results = index.search(&[5.05, 5.05], 2).unwrap();
        let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
        assert!(ids.contains(&1) && ids.contains(&2));
        // The old position must be gone.
        let far = index.search(&[0.0, 0.0], 1).unwrap();
        assert!(far[0].distance > 1.0, "the stale vector for id 1 is still in the graph");
    }

    #[test]
    fn compaction_drops_tombstones_and_preserves_results() {
        let (mut index, data) = build(500, 32, Metric::Cosine);
        for id in 0..100u64 {
            index.remove(id);
        }
        assert_eq!(index.capacity_used(), 500);

        let before = index.search(&data[400], 10).unwrap();
        index.compact().unwrap();

        assert_eq!(index.len(), 400);
        assert_eq!(index.capacity_used(), 400, "tombstones must be gone after compaction");
        let after = index.search(&data[400], 10).unwrap();
        assert_eq!(
            before.iter().map(|n| n.id).collect::<Vec<_>>(),
            after.iter().map(|n| n.id).collect::<Vec<_>>(),
            "compaction must not change what the index returns"
        );
    }

    #[test]
    fn index_round_trips_through_bytes() {
        let (index, data) = build(300, 16, Metric::Cosine);
        let bytes = index.to_bytes().unwrap();
        let restored = Hnsw::from_bytes(&bytes).unwrap();

        assert_eq!(restored.len(), index.len());
        let original = index.search(&data[7], 5).unwrap();
        let reloaded = restored.search(&data[7], 5).unwrap();
        assert_eq!(
            original.iter().map(|n| n.id).collect::<Vec<_>>(),
            reloaded.iter().map(|n| n.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn index_round_trips_through_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("index.bin");
        let (index, data) = build(200, 8, Metric::L2);
        index.save(&path).unwrap();

        let restored = Hnsw::load(&path).unwrap();
        assert_eq!(restored.len(), 200);
        assert_eq!(restored.search(&data[3], 1).unwrap()[0].id, 3);
    }

    #[test]
    fn builds_are_deterministic() {
        let (first, _) = build(200, 16, Metric::Cosine);
        let (second, _) = build(200, 16, Metric::Cosine);
        assert_eq!(
            first.to_bytes().unwrap(),
            second.to_bytes().unwrap(),
            "the same input must produce a byte-identical index"
        );
    }

    #[test]
    fn requesting_more_results_than_exist_returns_everything() {
        let (index, data) = build(10, 4, Metric::Cosine);
        let results = index.search(&data[0], 100).unwrap();
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn requesting_zero_results_returns_nothing() {
        let (index, data) = build(10, 4, Metric::Cosine);
        assert!(index.search(&data[0], 0).unwrap().is_empty());
    }

    #[test]
    fn every_live_vector_is_reachable() {
        // A graph that has become disconnected still answers queries, just
        // badly — this catches that failure directly.
        let (index, data) = build(400, 16, Metric::Cosine);
        for (i, vector) in data.iter().enumerate() {
            let results = index.search_with_ef(vector, 1, 100).unwrap();
            assert_eq!(
                results[0].id, i as u64,
                "vector {i} was not reachable through the graph"
            );
        }
    }

    #[test]
    fn duplicate_vectors_are_all_retained() {
        let mut index = Hnsw::new(HnswConfig::new(3).metric(Metric::L2));
        for id in 0..10u64 {
            index.insert(id, &[1.0, 1.0, 1.0]).unwrap();
        }
        assert_eq!(index.len(), 10);
        let results = index.search(&[1.0, 1.0, 1.0], 10).unwrap();
        assert_eq!(results.len(), 10, "identical vectors must not collapse into one node");
    }
}
