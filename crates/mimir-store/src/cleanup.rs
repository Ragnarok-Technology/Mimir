//! Transcript cleanup helpers and anti-hallucination guardrails.

/// Validates an LLM-cleaned utterance against `raw` to prevent hallucinations,
/// truncations, or meta-chatter from overwriting the original transcript.
pub fn sanitize_cleanup_edit(raw: &str, candidate: &str) -> String {
    let raw_trimmed = raw.trim();
    if raw_trimmed.is_empty() {
        return String::new();
    }

    let cleaned = candidate
        .trim()
        .trim_matches('"')
        .trim_matches('`')
        .lines()
        .next()
        .unwrap_or("")
        .trim();

    if cleaned.is_empty() {
        return raw_trimmed.to_string();
    }

    let lower = cleaned.to_lowercase();
    const CHATTER_PREFIXES: [&str; 6] = [
        "here is",
        "here's",
        "cleaned:",
        "corrected:",
        "i cannot",
        "as an ai",
    ];
    if CHATTER_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return raw_trimmed.to_string();
    }

    let raw_words = raw_trimmed.split_whitespace().count();
    let new_words = cleaned.split_whitespace().count();
    if raw_words >= 4 {
        let ratio = new_words as f32 / raw_words as f32;
        if !(0.4..=1.8).contains(&ratio) {
            return raw_trimmed.to_string();
        }
    }

    cleaned.to_string()
}

/// Fast, deterministic baseline cleanup (stutter collapse + keyword casing + sentence capitalization)
/// that always runs safely even before Qwen is invoked.
pub fn basic_heuristic_cleanup(raw: &str, keywords: &[String]) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }

    // Collapse immediate duplicate words (e.g. "the the", "I I") case-insensitively.
    let mut deduped: Vec<String> = Vec::with_capacity(words.len());
    for w in words {
        let same_as_prev = deduped
            .last()
            .is_some_and(|prev| prev.eq_ignore_ascii_case(w));
        if !same_as_prev {
            deduped.push(w.to_string());
        }
    }

    // Restore exact casing for configured keywords.
    for token in &mut deduped {
        let stripped = token.trim_matches(|c: char| !c.is_alphanumeric());
        if let Some(kw) = keywords.iter().find(|k| k.eq_ignore_ascii_case(stripped)) {
            *token = token.replacen(stripped, kw, 1);
        }
    }

    let mut out = deduped.join(" ");
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Builds the system and user prompt pair for Qwen 2.5 0.5B Instruct transcript cleanup.
pub fn build_qwen_cleanup_prompt(raw: &str, keywords: &[String]) -> (String, String) {
    let kw_hint = if keywords.is_empty() {
        String::new()
    } else {
        format!(
            " Domain terms to preserve/spell accurately: {}.",
            keywords.join(", ")
        )
    };
    let system = format!(
        "You are a conservative speech-to-text transcript editor. Fix obvious phonetic mishearings, punctuation, and capitalization while preserving the speaker's exact meaning and wording.{kw_hint} Output ONLY the corrected transcript line with no explanations or quotes."
    );
    (system, raw.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_hallucinated_expansion_and_chatter() {
        let raw = "yeah let's deploy mimir tomorrow morning";
        assert_eq!(
            sanitize_cleanup_edit(raw, "Here is the cleaned transcript: deploy"),
            raw
        );
        assert_eq!(
            sanitize_cleanup_edit(raw, "one two three four five six seven eight nine ten eleven twelve"),
            raw
        );
        assert_eq!(
            sanitize_cleanup_edit(raw, "Yeah, let's deploy Mimir tomorrow morning."),
            "Yeah, let's deploy Mimir tomorrow morning."
        );
    }

    #[test]
    fn heuristic_collapses_stutters_and_restores_keywords() {
        let cleaned = basic_heuristic_cleanup(
            "so we we deployed mimir on kubernetes",
            &["Mimir".into(), "Kubernetes".into()],
        );
        assert_eq!(cleaned, "So we deployed Mimir on Kubernetes");
    }
}
