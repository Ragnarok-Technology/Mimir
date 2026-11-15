//! Opt-in, strictly local first-pass transcript cleanup with anti-hallucination guardrails.

use serde_json::json;
use std::time::Duration;

/// Returns true only if `url` targets loopback (`127.0.0.1`, `localhost`, or `[::1]`).
pub fn is_loopback_endpoint(url: &str) -> bool {
    let trimmed = url.trim().to_ascii_lowercase();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(&trimmed);
    let host_port = rest.split('/').next().unwrap_or("");
    let host = if host_port.starts_with('[') {
        host_port.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Enforces anti-overcorrection bounds: rejects edits that diverge too far in word count or characters.
pub fn sanitize_cleanup_edit(raw: &str, candidate: &str) -> String {
    let raw_t = raw.trim();
    let cand_t = candidate.trim().trim_matches('"').trim();
    if raw_t.is_empty() || cand_t.is_empty() {
        return raw_t.to_string();
    }
    let raw_words = raw_t.split_whitespace().count();
    let cand_words = cand_t.split_whitespace().count();
    let max_word_delta = (raw_words / 4).max(2);
    if raw_words.abs_diff(cand_words) > max_word_delta {
        return raw_t.to_string();
    }
    let sim = char_similarity(&raw_t.to_ascii_lowercase(), &cand_t.to_ascii_lowercase());
    if sim < 0.45 {
        return raw_t.to_string();
    }
    cand_t.to_string()
}

/// Deterministic, sub-millisecond keyword casing & close-typo repair.
pub fn apply_keyword_phonetic_cleanup(raw: &str, keywords: &[String]) -> String {
    if keywords.is_empty() || raw.trim().is_empty() {
        return raw.to_string();
    }
    let single_kws: Vec<&str> = keywords
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.contains(' '))
        .collect();
    if single_kws.is_empty() {
        return raw.to_string();
    }

    let mut out_tokens = Vec::new();
    for tok in raw.split_whitespace() {
        let prefix_len = tok
            .find(|c: char| c.is_alphanumeric())
            .unwrap_or(tok.len());
        let suffix_start = tok
            .rfind(|c: char| c.is_alphanumeric())
            .map(|i| i + tok[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1))
            .unwrap_or(prefix_len);
        if suffix_start <= prefix_len {
            out_tokens.push(tok.to_string());
            continue;
        }
        let prefix = &tok[..prefix_len];
        let core = &tok[prefix_len..suffix_start];
        let suffix = &tok[suffix_start..];
        let core_low = core.to_ascii_lowercase();

        let mut replacement: Option<&str> = None;
        let mut best_sim = 0.0f32;
        for &kw in &single_kws {
            let kw_low = kw.to_ascii_lowercase();
            if core_low == kw_low {
                replacement = Some(kw);
                break;
            }
            if core_low.len() >= 4 && kw_low.len() >= 4 {
                let sim = char_similarity(&core_low, &kw_low);
                if sim >= 0.78 && sim > best_sim {
                    best_sim = sim;
                    replacement = Some(kw);
                }
            }
        }
        if let Some(rep) = replacement {
            out_tokens.push(format!("{prefix}{rep}{suffix}"));
        } else {
            out_tokens.push(tok.to_string());
        }
    }
    out_tokens.join(" ")
}

/// Runs Stage 1 (keyword cleanup) and optional Stage 2 (loopback small LLM cleanup) with guardrails.
pub fn cleanup_utterance_local(
    raw: &str,
    context_lines: &[String],
    keywords: &[String],
    endpoint: Option<&str>,
    model: Option<&str>,
) -> String {
    let stage1 = apply_keyword_phonetic_cleanup(raw, keywords);
    let Some(ep) = endpoint.map(str::trim).filter(|s| !s.is_empty()) else {
        return stage1;
    };
    if !is_loopback_endpoint(ep) {
        return stage1;
    }

    let url = format!("{}/v1/chat/completions", ep.trim_end_matches('/'));
    let model_name = model.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("qwen2.5:0.5b");
    let ctx_block = if context_lines.is_empty() {
        String::new()
    } else {
        format!("Recent context:\n{}\n\n", context_lines.join("\n"))
    };
    let kw_block = if keywords.is_empty() {
        String::new()
    } else {
        format!("Keywords: {}\n\n", keywords.join(", "))
    };
    let prompt = format!(
        "{ctx_block}{kw_block}Fix only obvious phonetic ASR typos, punctuation, or proper nouns in this single line. Do not paraphrase, summarize, or add/remove clauses. Return ONLY the corrected text:\n{stage1}"
    );

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .new_agent();

    let body = json!({
        "model": model_name,
        "temperature": 0.0,
        "messages": [
            { "role": "system", "content": "You are a minimal ASR typo fixer. Output only the corrected line." },
            { "role": "user", "content": prompt }
        ]
    });

    let Ok(payload) = serde_json::to_vec(&body) else {
        return stage1;
    };
    let Ok(mut resp) = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send(&payload)
    else {
        return stage1;
    };
    let Ok(raw_body) = resp.body_mut().read_to_string() else {
        return stage1;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw_body) else {
        return stage1;
    };
    let Some(reply) = parsed
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
    else {
        return stage1;
    };

    sanitize_cleanup_edit(&stage1, reply)
}

fn char_similarity(a: &str, b: &str) -> f32 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(&a_chars, &b_chars);
    1.0 - (dist as f32 / max_len as f32)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_cleanup_edit_rejects_hallucinations_and_keeps_minor_repairs() {
        let raw = "Now Strong Duffy got on and said FAA technicians are investigating.";
        let good = "Now Sean Duffy got on and said FAA technicians are investigating.";
        assert_eq!(sanitize_cleanup_edit(raw, good), good);

        let hallucinated = "Summary: The Secretary of Transportation announced an investigation into the outage and recommended passengers rebook.";
        assert_eq!(sanitize_cleanup_edit(raw, hallucinated), raw);
    }

    #[test]
    fn keyword_cleanup_fixes_casing_and_close_phonetic_typos() {
        let raw = "Now, why is eram important for L3Haris?";
        let kws = vec!["ERAM".into(), "L3Harris".into()];
        assert_eq!(
            apply_keyword_phonetic_cleanup(raw, &kws),
            "Now, why is ERAM important for L3Harris?"
        );
    }

    #[test]
    fn loopback_check_rejects_remote_hosts() {
        assert!(is_loopback_endpoint("http://127.0.0.1:11434"));
        assert!(is_loopback_endpoint("http://localhost:8080"));
        assert!(!is_loopback_endpoint("https://api.openai.com"));
        assert!(!is_loopback_endpoint("http://192.168.1.50:11434"));
    }
}
