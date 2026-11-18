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
    const CHATTER_PREFIXES: [&str; 9] = [
        "here is",
        "here's",
        "cleaned:",
        "corrected:",
        "previous:",
        "target:",
        "next:",
        "i cannot",
        "as an ai",
    ];
    if CHATTER_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return raw_trimmed.to_string();
    }

    let raw_words = raw_trimmed.split_whitespace().count();
    let new_words = cleaned.split_whitespace().count();
    if raw_words <= 3 && new_words < raw_words {
        return raw_trimmed.to_string();
    }
    if raw_words >= 4 {
        let ratio = new_words as f32 / raw_words as f32;
        if !(0.55..=1.6).contains(&ratio) {
            return raw_trimmed.to_string();
        }
    }
    // Reject edits that append/invent more than 2 extra words or drop too much of raw.
    if new_words > raw_words + 2 {
        return raw_trimmed.to_string();
    }
    let raw_tokens: std::collections::HashSet<String> = raw_trimmed
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let new_tokens: std::collections::HashSet<String> = cleaned
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if !raw_tokens.is_empty() {
        let kept = raw_tokens.intersection(&new_tokens).count();
        if kept * 2 < raw_tokens.len() {
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
        "You are a conservative speech-to-text transcript editor. Fix obvious phonetic ASR mishearings, split compound words, punctuation, and capitalization while keeping every spoken word (do not drop leading words like 'or', 'and', or 'so').{kw_hint} Output ONLY the corrected transcript line with no explanations or quotes."
    );
    (system, raw.trim().to_string())
}

/// Kept for API compatibility; delegates to single-line `build_qwen_cleanup_prompt`
/// so small 0.5B models never leak or duplicate neighboring utterances.
pub fn build_qwen_cleanup_prompt_with_context(
    _prev: Option<&str>,
    raw: &str,
    _next: Option<&str>,
    keywords: &[String],
) -> (String, String) {
    build_qwen_cleanup_prompt(raw, keywords)
}

/// Returns `true` if `title` is missing or looks like a default/timestamped placeholder.
pub fn is_default_or_timestamped_title(title: Option<&str>) -> bool {
    let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let lower = t.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "untitled" | "untitled session" | "new session" | "recording"
    ) {
        return true;
    }
    if lower.starts_with("session ") || lower.starts_with("recording ") {
        let rest = lower.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
        if rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '_' | ':' | ' ' | 't' | 'z'))
        {
            return true;
        }
    }
    t.len() >= 10
        && t.as_bytes()[0..4].iter().all(u8::is_ascii_digit)
        && t.as_bytes()[4] == b'-'
        && t.as_bytes()[5..7].iter().all(u8::is_ascii_digit)
        && t.as_bytes()[7] == b'-'
}

fn is_filler_utterance(text: &str) -> bool {
    let norm: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .trim()
        .to_ascii_lowercase();
    matches!(
        norm.as_str(),
        "" | "hi"
            | "hey"
            | "hello"
            | "yeah"
            | "yep"
            | "yes"
            | "no"
            | "ok"
            | "okay"
            | "um"
            | "uh"
            | "thanks"
            | "thank you"
            | "can you hear me"
            | "testing"
            | "test"
    )
}

/// Selects a compact, high-signal transcript excerpt (~800 chars) for lightweight auto-titling.
pub fn select_salient_title_excerpt(
    utterances: &[mimir_core::Utterance],
    keywords: &[String],
    max_chars: usize,
) -> String {
    let non_empty: Vec<(usize, &str, usize)> = utterances
        .iter()
        .enumerate()
        .filter_map(|(idx, u)| {
            let t = u.text.trim();
            if t.is_empty() || is_filler_utterance(t) {
                return None;
            }
            let lower = t.to_ascii_lowercase();
            let kw_hits = keywords
                .iter()
                .filter(|k| !k.is_empty() && lower.contains(&k.to_ascii_lowercase()))
                .count();
            // Favor early context + keyword hits + substantive length (capped).
            let early_bonus = if idx < 3 { 20 } else { 0 };
            let score = kw_hits * 50 + early_bonus + t.len().min(140);
            Some((idx, t, score))
        })
        .collect();

    if non_empty.is_empty() {
        return utterances
            .iter()
            .map(|u| u.text.trim())
            .filter(|s| !s.is_empty())
            .take(5)
            .collect::<Vec<_>>()
            .join(" ");
    }

    let mut ranked = non_empty.clone();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let mut picked: Vec<(usize, &str)> = Vec::new();
    let mut total_len = 0usize;
    let budget = max_chars.max(200);
    for (idx, text, _) in ranked {
        if total_len + text.len() + 1 > budget && !picked.is_empty() {
            continue;
        }
        total_len += text.len() + 1;
        picked.push((idx, text));
        if total_len >= budget {
            break;
        }
    }
    picked.sort_by_key(|(idx, _)| *idx);
    picked
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Builds a tiny single-pass prompt (for `max_new_tokens = 16`) to title a session.
pub fn build_qwen_auto_title_prompt(excerpt: &str, keywords: &[String]) -> (String, String) {
    let kw_hint = if keywords.is_empty() {
        String::new()
    } else {
        format!(" Keywords: {}.", keywords.join(", "))
    };
    let system = format!(
        "Generate a concise 2 to 6 word topic title for this transcript excerpt.{kw_hint} Output ONLY the plain title text with no quotes, no markdown, no labels, and no trailing punctuation."
    );
    (system, excerpt.to_string())
}

fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "the" | "and" | "for" | "that" | "this" | "with" | "from" | "have" | "will" | "what"
            | "when" | "where" | "which" | "about" | "into" | "your" | "they" | "them" | "their"
            | "there" | "then" | "than" | "just" | "like" | "know" | "yeah" | "okay" | "right"
            | "going" | "because" | "could" | "would" | "should" | "were" | "been" | "being"
            | "some" | "more" | "very" | "much" | "also" | "only" | "here" | "now" | "well"
    )
}

/// Validates a Qwen-generated title against boilerplate and ensures content overlap.
pub fn sanitize_generated_title(
    raw: &str,
    excerpt: &str,
    keywords: &[String],
) -> Option<String> {
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;

    let mut s = first_line
        .trim_start_matches('#')
        .trim_start_matches('-')
        .trim_start_matches('*')
        .trim();

    for prefix in ["Title:", "title:", "Topic:", "topic:", "Subject:", "subject:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
        }
    }
    s = s
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .trim_end_matches(|c: char| matches!(c, ':' | '.' | '!' | '?' | ',' | ';'))
        .trim();

    if s.is_empty() || s.len() > 64 {
        return None;
    }

    let lower = s.to_ascii_lowercase();
    const BOILERPLATE: [&str; 12] = [
        "open questions",
        "key decisions",
        "discussion points",
        "action items",
        "next steps",
        "meeting notes",
        "untitled",
        "summary",
        "transcript",
        "here is",
        "i cannot",
        "as an ai",
    ];
    if BOILERPLATE.iter().any(|b| lower == *b || lower.starts_with(b)) {
        return None;
    }

    let words: Vec<&str> = s.split_whitespace().collect();
    if !(2..=8).contains(&words.len()) {
        return None;
    }

    let mut source_tokens: std::collections::HashSet<String> = excerpt
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase())
        .filter(|w| w.len() >= 3 && !is_stopword(w))
        .collect();
    for kw in keywords {
        for part in kw.split_whitespace() {
            let clean = part.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
            if clean.len() >= 2 {
                source_tokens.insert(clean);
            }
        }
    }

    let has_overlap = words.iter().any(|w| {
        let clean = w.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        source_tokens.contains(&clean)
    });
    if !source_tokens.is_empty() && !has_overlap {
        return None;
    }

    Some(s.to_string())
}

/// Fast, deterministic extractive title fallback when Qwen is unavailable or rejected.
pub fn extractive_fallback_title(
    utterances: &[mimir_core::Utterance],
    keywords: &[String],
) -> Option<String> {
    let full = utterances
        .iter()
        .map(|u| u.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if full.is_empty() {
        return None;
    }
    let full_lower = full.to_ascii_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for kw in keywords {
        let k = kw.trim();
        if !k.is_empty()
            && full_lower.contains(&k.to_ascii_lowercase())
            && !terms.iter().any(|t| t.eq_ignore_ascii_case(k))
        {
            terms.push(k.to_string());
            if terms.len() >= 3 {
                break;
            }
        }
    }

    if terms.len() < 3 {
        let mut counts: std::collections::HashMap<String, (usize, usize, String)> =
            std::collections::HashMap::new();
        for (pos, raw_tok) in full.split_whitespace().enumerate() {
            let clean = raw_tok.trim_matches(|c: char| !c.is_alphanumeric());
            let lower = clean.to_ascii_lowercase();
            if clean.len() < 4 || is_stopword(&lower) {
                continue;
            }
            let bonus = if clean.chars().next().is_some_and(char::is_uppercase) {
                2
            } else {
                1
            };
            counts
                .entry(lower)
                .and_modify(|e| e.0 += bonus)
                .or_insert((bonus, pos, clean.to_string()));
        }
        let mut ranked: Vec<(usize, usize, String)> = counts.into_values().collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, _, display) in ranked {
            if !terms.iter().any(|t| t.eq_ignore_ascii_case(&display)) {
                let mut chars = display.chars();
                let titled = match chars.next() {
                    Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
                    None => display,
                };
                terms.push(titled);
                if terms.len() >= 4 {
                    break;
                }
            }
        }
    }

    if terms.len() >= 2 {
        Some(terms.join(" "))
    } else {
        None
    }
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
            sanitize_cleanup_edit(raw, "Previous: something else"),
            raw
        );
        assert_eq!(
            sanitize_cleanup_edit(raw, "one two three four five six seven eight nine ten eleven twelve"),
            raw
        );
        assert_eq!(
            sanitize_cleanup_edit("It's overwhelmed.", "or New York."),
            "It's overwhelmed."
        );
        assert_eq!(
            sanitize_cleanup_edit(
                "No other airplane lands.",
                "No other airplane lands. We clear out the system."
            ),
            "No other airplane lands."
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

    #[test]
    fn auto_title_sanitizer_rejects_boilerplate_and_requires_overlap() {
        let excerpt = "L3Harris pushed an ERAM update that triggered a nationwide FAA groundstop.";
        let kws = vec!["ERAM".into(), "L3Harris".into()];
        assert_eq!(
            sanitize_generated_title("Open Questions:", excerpt, &kws),
            None
        );
        assert_eq!(
            sanitize_generated_title("Key Decisions", excerpt, &kws),
            None
        );
        assert_eq!(
            sanitize_generated_title("Quarterly Sales Pipeline", excerpt, &kws),
            None
        );
        assert_eq!(
            sanitize_generated_title("Title: \"FAA ERAM Outage Update.\"", excerpt, &kws),
            Some("FAA ERAM Outage Update".to_string())
        );
    }

    #[test]
    fn default_title_detection_and_extractive_fallback_work() {
        assert!(is_default_or_timestamped_title(None));
        assert!(is_default_or_timestamped_title(Some("Untitled Session")));
        assert!(is_default_or_timestamped_title(Some("2026-11-18 11:00")));
        assert!(!is_default_or_timestamped_title(Some("Weekly Architecture Sync")));

        let utts = vec![
            mimir_core::Utterance {
                id: uuid::Uuid::new_v4(),
                speaker_id: "host".into(),
                channel: mimir_core::ChannelId::Mic,
                start_ms: 0,
                end_ms: 1000,
                text: "hey".into(),
                raw_text: None,
                language: "en".into(),
                avg_confidence: 0.9,
                words: vec![],
            },
            mimir_core::Utterance {
                id: uuid::Uuid::new_v4(),
                speaker_id: "host".into(),
                channel: mimir_core::ChannelId::Mic,
                start_ms: 1000,
                end_ms: 5000,
                text: "Deploying Kubernetes ingress controllers on Mimir staging cluster.".into(),
                raw_text: None,
                language: "en".into(),
                avg_confidence: 0.9,
                words: vec![],
            },
        ];
        let excerpt = select_salient_title_excerpt(&utts, &["Kubernetes".into()], 800);
        assert!(!excerpt.starts_with("hey\n"));
        assert!(excerpt.contains("Kubernetes"));

        let fallback = extractive_fallback_title(&utts, &["Kubernetes".into(), "Mimir".into()]);
        assert!(fallback.is_some());
        assert!(fallback.unwrap().contains("Kubernetes"));
    }
}
