//! Speaker attribution, embedding clustering, ghost-bleed deduplication, and timeline merging.

pub mod onnx;

use mimir_core::{ChannelId, SpeakerInfo, Utterance};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub use onnx::{OnnxSpeakerEmbedder, SpeakerEmbedder};

/// Configuration for speaker diarization and bleed deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiarizerConfig {
    /// Display label for the primary local microphone speaker.
    pub host_label: String,
    /// When true, also cluster multiple speakers on the `Mic` channel (e.g. in-room meetings).
    pub diarize_mic: bool,
    /// Minimum cosine similarity in `[0.0, 1.0]` to merge into an existing speaker.
    pub similarity_threshold: f32,
    /// Time window (ms) around a system utterance to check for ghost mic bleed.
    pub ghost_window_ms: u64,
    /// Minimum Jaccard word overlap to classify a mic utterance as ghost bleed.
    pub ghost_overlap_threshold: f32,
}

impl Default for DiarizerConfig {
    fn default() -> Self {
        Self {
            host_label: "Host".to_string(),
            diarize_mic: false,
            similarity_threshold: 0.72,
            ghost_window_ms: 500,
            ghost_overlap_threshold: 0.65,
        }
    }
}

/// Enrolled local voiceprint for recognizing recurring remote speakers across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Voiceprint {
    pub id: String,
    pub label: String,
    pub centroid: Vec<f32>,
}

#[derive(Debug, Clone)]
struct RemoteCluster {
    info: SpeakerInfo,
    centroid: Vec<f32>,
    count: usize,
}

/// Stateful per-session speaker tracker and timeline merger.
pub struct Diarizer {
    config: DiarizerConfig,
    host_info: SpeakerInfo,
    clusters: Vec<RemoteCluster>,
    mic_clusters: Vec<RemoteCluster>,
    next_remote_idx: usize,
    next_mic_idx: usize,
}

impl Diarizer {
    pub fn new(config: DiarizerConfig, enrolled_voiceprints: Vec<Voiceprint>) -> Self {
        let host_info = SpeakerInfo {
            id: "host".to_string(),
            label: config.host_label.clone(),
            channel: ChannelId::Mic,
            is_host: true,
        };

        let clusters = enrolled_voiceprints
            .into_iter()
            .filter_map(|vp| {
                let norm = l2_normalize(&vp.centroid)?;
                Some(RemoteCluster {
                    info: SpeakerInfo {
                        id: vp.id,
                        label: vp.label,
                        channel: ChannelId::System,
                        is_host: false,
                    },
                    centroid: norm,
                    count: 1,
                })
            })
            .collect();

        Self {
            config,
            host_info,
            clusters,
            mic_clusters: Vec::new(),
            next_remote_idx: 1,
            next_mic_idx: 1,
        }
    }

    /// Assigns a speaker for an utterance on `channel` using an optional speech embedding.
    pub fn assign_speaker(
        &mut self,
        channel: ChannelId,
        embedding: Option<&[f32]>,
    ) -> SpeakerInfo {
        if channel == ChannelId::Mic {
            if !self.config.diarize_mic {
                return self.host_info.clone();
            }
            return self.assign_mic_speaker(embedding);
        }

        let Some(norm_emb) = embedding.and_then(l2_normalize) else {
            return self.fallback_remote_speaker();
        };

        let mut best_idx: Option<usize> = None;
        let mut best_sim = f32::NEG_INFINITY;

        for (idx, cluster) in self.clusters.iter().enumerate() {
            if cluster.centroid.len() != norm_emb.len() {
                continue;
            }
            let sim = cosine_similarity(&cluster.centroid, &norm_emb);
            if sim > best_sim {
                best_sim = sim;
                best_idx = Some(idx);
            }
        }

        if let Some(idx) = best_idx
            && best_sim >= self.config.similarity_threshold
        {
            let cluster = &mut self.clusters[idx];
            update_centroid(&mut cluster.centroid, cluster.count, &norm_emb);
            cluster.count += 1;
            return cluster.info.clone();
        }

        let speaker_num = self.next_remote_idx;
        self.next_remote_idx += 1;
        let info = SpeakerInfo {
            id: format!("remote_{speaker_num}"),
            label: format!("Speaker {speaker_num}"),
            channel: ChannelId::System,
            is_host: false,
        };
        self.clusters.push(RemoteCluster {
            info: info.clone(),
            centroid: norm_emb,
            count: 1,
        });
        info
    }

    /// Renames a tracked speaker in-place; returns `true` if the speaker was found.
    pub fn rename_speaker(&mut self, speaker_id: &str, new_label: &str) -> bool {
        if speaker_id == "host" {
            self.host_info.label = new_label.to_string();
            return true;
        }
        if let Some(c) = self.clusters.iter_mut().find(|c| c.info.id == speaker_id) {
            c.info.label = new_label.to_string();
            return true;
        }
        if let Some(c) = self.mic_clusters.iter_mut().find(|c| c.info.id == speaker_id) {
            c.info.label = new_label.to_string();
            return true;
        }
        false
    }

    /// Returns all speakers currently known to the diarizer.
    pub fn speakers_map(&self) -> HashMap<String, SpeakerInfo> {
        let mut map = HashMap::new();
        if self.mic_clusters.is_empty() {
            map.insert(self.host_info.id.clone(), self.host_info.clone());
        } else {
            for c in &self.mic_clusters {
                map.insert(c.info.id.clone(), c.info.clone());
            }
        }
        for c in &self.clusters {
            map.insert(c.info.id.clone(), c.info.clone());
        }
        map
    }

    fn assign_mic_speaker(&mut self, embedding: Option<&[f32]>) -> SpeakerInfo {
        let Some(norm_emb) = embedding.and_then(l2_normalize) else {
            return self.host_info.clone();
        };

        let mut best_idx: Option<usize> = None;
        let mut best_sim = f32::NEG_INFINITY;
        for (idx, cluster) in self.mic_clusters.iter().enumerate() {
            if cluster.centroid.len() != norm_emb.len() {
                continue;
            }
            let sim = cosine_similarity(&cluster.centroid, &norm_emb);
            if sim > best_sim {
                best_sim = sim;
                best_idx = Some(idx);
            }
        }

        if let Some(idx) = best_idx
            && best_sim >= self.config.similarity_threshold
        {
            let cluster = &mut self.mic_clusters[idx];
            update_centroid(&mut cluster.centroid, cluster.count, &norm_emb);
            cluster.count += 1;
            return cluster.info.clone();
        }

        let idx = self.next_mic_idx;
        self.next_mic_idx += 1;
        let info = if idx == 1 {
            self.host_info.clone()
        } else {
            SpeakerInfo {
                id: format!("mic_{idx}"),
                label: format!("Local Speaker {idx}"),
                channel: ChannelId::Mic,
                is_host: false,
            }
        };
        self.mic_clusters.push(RemoteCluster {
            info: info.clone(),
            centroid: norm_emb,
            count: 1,
        });
        info
    }

    /// Detects whether a `Mic` utterance is likely acoustic bleed from a recent `System` utterance.
    pub fn is_ghost_bleed(&self, mic_utterance: &Utterance, recent_system: &[Utterance]) -> bool {
        if mic_utterance.channel != ChannelId::Mic {
            return false;
        }
        let mic_words = normalized_word_set(&mic_utterance.text);
        if mic_words.is_empty() {
            return false;
        }

        for sys in recent_system {
            if sys.channel != ChannelId::System {
                continue;
            }
            let overlaps_window = mic_utterance.start_ms <= sys.end_ms + self.config.ghost_window_ms
                && sys.start_ms <= mic_utterance.end_ms + self.config.ghost_window_ms;
            if !overlaps_window {
                continue;
            }
            let sys_words = normalized_word_set(&sys.text);
            let overlap = jaccard_similarity(&mic_words, &sys_words);
            if overlap >= self.config.ghost_overlap_threshold {
                return true;
            }
        }
        false
    }

    fn fallback_remote_speaker(&mut self) -> SpeakerInfo {
        if let Some(first) = self.clusters.first() {
            return first.info.clone();
        }
        let speaker_num = self.next_remote_idx;
        self.next_remote_idx += 1;
        let info = SpeakerInfo {
            id: format!("remote_{speaker_num}"),
            label: format!("Speaker {speaker_num}"),
            channel: ChannelId::System,
            is_host: false,
        };
        self.clusters.push(RemoteCluster {
            info: info.clone(),
            centroid: Vec::new(),
            count: 0,
        });
        info
    }
}

/// Inserts an utterance into a chronologically sorted timeline (by `start_ms`, then `end_ms`).
pub fn insert_utterance_chronological(timeline: &mut Vec<Utterance>, utterance: Utterance) {
    let pos = timeline.partition_point(|u| {
        (u.start_ms, u.end_ms) <= (utterance.start_ms, utterance.end_ms)
    });
    timeline.insert(pos, utterance);
}

/// Mean-pools flattened `[frames * dim]` 80 ms Whistle embeddings into a single `[dim]` vector.
pub fn mean_pool_frames(flattened: &[f32], frame_dim: usize) -> Option<Vec<f32>> {
    if frame_dim == 0 || flattened.len() < frame_dim || !flattened.len().is_multiple_of(frame_dim) {
        return None;
    }
    let frames = flattened.len() / frame_dim;
    let mut pooled = vec![0.0f32; frame_dim];
    for chunk in flattened.chunks_exact(frame_dim) {
        for (acc, &val) in pooled.iter_mut().zip(chunk.iter()) {
            *acc += val;
        }
    }
    for acc in &mut pooled {
        *acc /= frames as f32;
    }
    l2_normalize(&pooled)
}

fn l2_normalize(v: &[f32]) -> Option<Vec<f32>> {
    if v.is_empty() {
        return None;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= 1e-8 || !norm.is_finite() {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn update_centroid(centroid: &mut [f32], count: usize, new_vec: &[f32]) {
    let n = count as f32;
    for (c, &x) in centroid.iter_mut().zip(new_vec.iter()) {
        *c = (*c * n + x) / (n + 1.0);
    }
    if let Some(normed) = l2_normalize(centroid) {
        centroid.copy_from_slice(&normed);
    }
}

fn normalized_word_set(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 { 0.0 } else { intersection / union }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_utterance(channel: ChannelId, start_ms: u64, end_ms: u64, text: &str) -> Utterance {
        Utterance {
            id: Uuid::new_v4(),
            speaker_id: if channel == ChannelId::Mic {
                "host".into()
            } else {
                "remote_1".into()
            },
            channel,
            start_ms,
            end_ms,
            text: text.into(),
            raw_text: None,
            language: "en".into(),
            avg_confidence: 0.95,
            words: Vec::new(),
        }
    }

    #[test]
    fn mic_always_maps_to_host_when_diarize_mic_is_disabled() {
        let mut diarizer = Diarizer::new(DiarizerConfig::default(), vec![]);
        let spk = diarizer.assign_speaker(ChannelId::Mic, Some(&[1.0, 0.0, 0.0]));
        assert_eq!(spk.id, "host");
        assert!(spk.is_host);
    }

    #[test]
    fn diarize_mic_separates_multiple_local_speakers() {
        let cfg = DiarizerConfig {
            diarize_mic: true,
            ..Default::default()
        };
        let mut diarizer = Diarizer::new(cfg, vec![]);
        let s1 = diarizer.assign_speaker(ChannelId::Mic, Some(&[1.0, 0.0, 0.0]));
        let s2 = diarizer.assign_speaker(ChannelId::Mic, Some(&[0.0, 1.0, 0.0]));
        assert_eq!(s1.id, "host");
        assert_eq!(s2.id, "mic_2");
        assert_eq!(s2.label, "Local Speaker 2");
    }

    #[test]
    fn orthogonal_embeddings_create_distinct_speakers_and_similar_ones_cluster() {
        let mut diarizer = Diarizer::new(DiarizerConfig::default(), vec![]);

        let s1 = diarizer.assign_speaker(ChannelId::System, Some(&[1.0, 0.0, 0.0]));
        let s2 = diarizer.assign_speaker(ChannelId::System, Some(&[0.0, 1.0, 0.0]));
        let s1_again = diarizer.assign_speaker(ChannelId::System, Some(&[0.95, 0.05, 0.0]));

        assert_eq!(s1.id, "remote_1");
        assert_eq!(s2.id, "remote_2");
        assert_eq!(s1_again.id, "remote_1");
    }

    #[test]
    fn enrolled_voiceprint_recognizes_named_colleague() {
        let vp = Voiceprint {
            id: "alice".into(),
            label: "Alice".into(),
            centroid: vec![0.0, 0.0, 1.0],
        };
        let mut diarizer = Diarizer::new(DiarizerConfig::default(), vec![vp]);
        let spk = diarizer.assign_speaker(ChannelId::System, Some(&[0.05, 0.0, 0.99]));
        assert_eq!(spk.id, "alice");
        assert_eq!(spk.label, "Alice");
    }

    #[test]
    fn ghost_bleed_suppresses_echo_but_preserves_crosstalk() {
        let diarizer = Diarizer::new(DiarizerConfig::default(), vec![]);
        let sys = vec![make_utterance(
            ChannelId::System,
            1000,
            2500,
            "Let's deploy the release tomorrow morning.",
        )];

        let ghost = make_utterance(
            ChannelId::Mic,
            1120,
            2550,
            "Let's deploy the release tomorrow morning",
        );
        assert!(diarizer.is_ghost_bleed(&ghost, &sys));

        let real_crosstalk = make_utterance(
            ChannelId::Mic,
            1200,
            2000,
            "Wait, we still need to run the migrations first.",
        );
        assert!(!diarizer.is_ghost_bleed(&real_crosstalk, &sys));
    }

    #[test]
    fn chronological_merge_orders_crosstalk_by_start_ms() {
        let mut timeline = Vec::new();
        insert_utterance_chronological(&mut timeline, make_utterance(ChannelId::System, 2000, 3000, "second"));
        insert_utterance_chronological(&mut timeline, make_utterance(ChannelId::Mic, 1000, 1800, "first"));
        insert_utterance_chronological(&mut timeline, make_utterance(ChannelId::System, 1500, 2200, "middle"));

        let texts: Vec<&str> = timeline.iter().map(|u| u.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "middle", "second"]);
    }

    #[test]
    fn mean_pool_frames_averages_and_normalizes() {
        let pooled = mean_pool_frames(&[1.0, 0.0, 0.0, 1.0], 2).unwrap();
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!((pooled[0] - expected).abs() < 1e-5);
        assert!((pooled[1] - expected).abs() < 1e-5);
    }
}
