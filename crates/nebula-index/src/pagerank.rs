//! Personalised PageRank over a sparse directed graph.

use serde::{Deserialize, Serialize};

/// PageRank parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PageRankConfig {
    /// Damping factor: the probability the random surfer follows an edge rather
    /// than teleporting. 0.85 is the value from the original paper and is what
    /// virtually every implementation uses.
    pub damping: f64,
    /// Stop once no node's rank moves by more than this in an iteration.
    pub tolerance: f64,
    /// Hard iteration cap, so a pathological graph cannot spin forever.
    pub max_iterations: usize,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self { damping: 0.85, tolerance: 1e-6, max_iterations: 100 }
    }
}

/// A sparse directed graph with weighted edges, ready to rank.
#[derive(Debug, Clone, Default)]
pub struct PageRank {
    /// `edges[from]` holds `(to, weight)` pairs.
    edges: Vec<Vec<(usize, f64)>>,
    /// Sum of outgoing weights per node, cached.
    out_weight: Vec<f64>,
}

impl PageRank {
    /// A graph with `node_count` nodes and no edges.
    pub fn new(node_count: usize) -> Self {
        Self { edges: vec![Vec::new(); node_count], out_weight: vec![0.0; node_count] }
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.edges.len()
    }

    /// Total number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.iter().map(Vec::len).sum()
    }

    /// Add a weighted edge `from -> to`.
    ///
    /// Repeated edges accumulate rather than replacing, so a caller can add one
    /// edge per reference and let the weights build up naturally.
    pub fn add_edge(&mut self, from: usize, to: usize, weight: f64) {
        if from >= self.edges.len() || to >= self.edges.len() || weight <= 0.0 {
            return;
        }
        if let Some(existing) = self.edges[from].iter_mut().find(|(t, _)| *t == to) {
            existing.1 += weight;
        } else {
            self.edges[from].push((to, weight));
        }
        self.out_weight[from] += weight;
    }

    /// Rank every node, returning a score per node summing to 1.
    ///
    /// `personalization` biases the teleport distribution: a surfer that
    /// teleports lands on those nodes rather than uniformly at random. Pass an
    /// empty slice for classic uniform PageRank.
    pub fn rank(&self, personalization: &[f64], config: &PageRankConfig) -> Vec<f64> {
        let n = self.edges.len();
        if n == 0 {
            return Vec::new();
        }

        // Normalise the teleport distribution, falling back to uniform if the
        // caller supplied nothing usable.
        let teleport: Vec<f64> = {
            let sum: f64 = personalization.iter().filter(|v| v.is_finite() && **v > 0.0).sum();
            if personalization.len() == n && sum > 0.0 {
                personalization
                    .iter()
                    .map(|v| if v.is_finite() && *v > 0.0 { v / sum } else { 0.0 })
                    .collect()
            } else {
                vec![1.0 / n as f64; n]
            }
        };

        let mut rank = teleport.clone();
        let mut next = vec![0.0f64; n];

        for _ in 0..config.max_iterations {
            next.iter_mut().for_each(|v| *v = 0.0);

            // Rank held by dangling nodes (no outgoing edges) would vanish; it
            // is redistributed over the teleport distribution instead, which is
            // what keeps the vector summing to 1.
            let mut dangling = 0.0f64;

            for node in 0..n {
                if self.out_weight[node] <= 0.0 {
                    dangling += rank[node];
                    continue;
                }
                let share = rank[node] / self.out_weight[node];
                for &(to, weight) in &self.edges[node] {
                    next[to] += share * weight;
                }
            }

            let mut delta = 0.0f64;
            for node in 0..n {
                let value = config.damping * (next[node] + dangling * teleport[node])
                    + (1.0 - config.damping) * teleport[node];
                delta += (value - rank[node]).abs();
                next[node] = value;
            }
            std::mem::swap(&mut rank, &mut next);

            if delta < config.tolerance {
                break;
            }
        }

        // Guard against drift from floating-point accumulation.
        let total: f64 = rank.iter().sum();
        if total > 0.0 {
            for value in &mut rank {
                *value /= total;
            }
        }
        rank
    }

    /// Rank with the default configuration and a uniform teleport distribution.
    pub fn rank_uniform(&self) -> Vec<f64> {
        self.rank(&[], &PageRankConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn ranks_sum_to_one() {
        let mut graph = PageRank::new(5);
        graph.add_edge(0, 1, 1.0);
        graph.add_edge(1, 2, 1.0);
        graph.add_edge(2, 0, 1.0);
        graph.add_edge(3, 0, 1.0);

        let ranks = graph.rank_uniform();
        assert!(approx(ranks.iter().sum::<f64>(), 1.0), "sum was {}", ranks.iter().sum::<f64>());
    }

    #[test]
    fn a_node_everything_points_at_ranks_highest() {
        let mut graph = PageRank::new(4);
        for from in 1..4 {
            graph.add_edge(from, 0, 1.0);
        }
        let ranks = graph.rank_uniform();
        let best = ranks
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(best, 0);
    }

    #[test]
    fn importance_flows_transitively() {
        // 2 and 3 both point at 1; only 1 points at 0. Node 0 should still
        // outrank 1, because it inherits everything 1 accumulated.
        let mut graph = PageRank::new(4);
        graph.add_edge(1, 0, 1.0);
        graph.add_edge(2, 1, 1.0);
        graph.add_edge(3, 1, 1.0);

        let ranks = graph.rank_uniform();
        assert!(ranks[0] > ranks[1], "0={:.4} 1={:.4}", ranks[0], ranks[1]);
        assert!(ranks[1] > ranks[2]);
    }

    #[test]
    fn an_isolated_graph_ranks_uniformly() {
        let graph = PageRank::new(4);
        let ranks = graph.rank_uniform();
        for rank in &ranks {
            assert!(approx(*rank, 0.25), "expected 0.25, got {rank}");
        }
    }

    #[test]
    fn an_empty_graph_produces_no_ranks() {
        assert!(PageRank::new(0).rank_uniform().is_empty());
    }

    #[test]
    fn dangling_nodes_do_not_leak_rank() {
        // Node 2 has no outgoing edges. Without redistribution, total rank would
        // bleed away every iteration.
        let mut graph = PageRank::new(3);
        graph.add_edge(0, 2, 1.0);
        graph.add_edge(1, 2, 1.0);

        let ranks = graph.rank(&[], &PageRankConfig { max_iterations: 500, ..Default::default() });
        assert!(approx(ranks.iter().sum::<f64>(), 1.0), "sum was {}", ranks.iter().sum::<f64>());
        assert!(ranks.iter().all(|r| *r > 0.0));
    }

    #[test]
    fn personalization_biases_the_result() {
        let mut graph = PageRank::new(4);
        graph.add_edge(0, 1, 1.0);
        graph.add_edge(2, 3, 1.0);

        let uniform = graph.rank_uniform();
        // Teleport only onto node 2, which feeds node 3.
        let personalized = graph.rank(&[0.0, 0.0, 1.0, 0.0], &PageRankConfig::default());

        assert!(
            personalized[3] > uniform[3],
            "personalising towards 2 must raise its dependent 3: {:.4} vs {:.4}",
            personalized[3],
            uniform[3]
        );
        assert!(personalized[1] < uniform[1]);
        assert!(approx(personalized.iter().sum::<f64>(), 1.0));
    }

    #[test]
    fn edge_weights_matter() {
        let mut graph = PageRank::new(3);
        graph.add_edge(0, 1, 9.0);
        graph.add_edge(0, 2, 1.0);

        let ranks = graph.rank_uniform();
        assert!(ranks[1] > ranks[2], "the heavier edge must carry more rank");
    }

    #[test]
    fn repeated_edges_accumulate_weight() {
        let mut graph = PageRank::new(3);
        for _ in 0..5 {
            graph.add_edge(0, 1, 1.0);
        }
        graph.add_edge(0, 2, 1.0);
        assert_eq!(graph.edge_count(), 2, "repeated edges merge rather than duplicating");

        let ranks = graph.rank_uniform();
        assert!(ranks[1] > ranks[2]);
    }

    #[test]
    fn out_of_range_and_non_positive_edges_are_ignored() {
        let mut graph = PageRank::new(2);
        graph.add_edge(5, 0, 1.0);
        graph.add_edge(0, 5, 1.0);
        graph.add_edge(0, 1, 0.0);
        graph.add_edge(0, 1, -3.0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn a_self_loop_does_not_break_convergence() {
        let mut graph = PageRank::new(3);
        graph.add_edge(0, 0, 1.0);
        graph.add_edge(1, 0, 1.0);
        graph.add_edge(2, 1, 1.0);

        let ranks = graph.rank_uniform();
        assert!(ranks.iter().all(|r| r.is_finite()));
        assert!(approx(ranks.iter().sum::<f64>(), 1.0));
    }

    #[test]
    fn results_are_deterministic() {
        let mut graph = PageRank::new(20);
        for i in 0..20 {
            graph.add_edge(i, (i * 7 + 3) % 20, 1.0 + (i as f64 % 3.0));
        }
        assert_eq!(graph.rank_uniform(), graph.rank_uniform());
    }

    #[test]
    fn a_malformed_personalization_vector_falls_back_to_uniform() {
        let mut graph = PageRank::new(3);
        graph.add_edge(0, 1, 1.0);

        // Wrong length, all zeros, and NaN all fall back rather than producing
        // NaN ranks.
        for bad in [vec![1.0], vec![0.0, 0.0, 0.0], vec![f64::NAN, 1.0, 1.0]] {
            let ranks = graph.rank(&bad, &PageRankConfig::default());
            assert!(ranks.iter().all(|r| r.is_finite()), "bad input {bad:?} produced {ranks:?}");
            assert!(approx(ranks.iter().sum::<f64>(), 1.0));
        }
    }

    #[test]
    fn convergence_stops_early_on_a_simple_graph() {
        let mut graph = PageRank::new(3);
        graph.add_edge(0, 1, 1.0);
        graph.add_edge(1, 2, 1.0);
        graph.add_edge(2, 0, 1.0);

        // A tight iteration cap and a loose tolerance must still converge to the
        // symmetric answer.
        let ranks = graph.rank(
            &[],
            &PageRankConfig { max_iterations: 5, tolerance: 1e-3, ..Default::default() },
        );
        for rank in &ranks {
            assert!((rank - 1.0 / 3.0).abs() < 0.01, "expected ~0.333, got {rank}");
        }
    }
}
