use crate::session::{BaseKind, SessionState, TokenStats};
use regex::Regex;

pub struct PatternMatcher {
    // Shell/generic ready prompts
    shell_ready: Regex,
    local_thinking: Regex,
    local_ready: Regex,
    local_waiting: Regex,
    // Claude-specific patterns
    claude_thinking: Regex,
    claude_ready: Regex,
    claude_waiting: Regex,
    // Codex patterns
    codex_ready: Regex,
    codex_waiting: Regex,
    // Generic waiting/error
    generic_waiting: Regex,
    generic_error: Regex,
    // llama.cpp startup line: "n_ctx = 8192"
    nctx_re: Regex,
    // Token / cost extraction (applied to full screen text)
    cost_re: Regex,
    tokens_in_re: Regex,
    tokens_out_re: Regex,
    tokens_total_re: Regex,
}

impl PatternMatcher {
    pub fn new() -> Self {
        Self {
            shell_ready: Regex::new(r"[\$#%>]\s*$").unwrap(),
            // opencode / omp / pi / aider / llama-cli: braille spinners, an
            // explicit working verb, or "esc to interrupt" style hints.
            local_thinking: Regex::new(
                r"⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|(?i)\b(thinking|working|generating|reasoning)\b\.\.\.|esc to interrupt|ctrl\+c to interrupt",
            )
            .unwrap(),
            // idle prompt markers used by local agent TUIs and llama-cli
            local_ready: Regex::new(r"^[>❯]\s*$|^\(\S+\)>\s*$").unwrap(),
            // opencode permission dialog ("△ Permission required" with
            // Allow once / Allow always / Reject options) and aider's
            // (Y)es/(N)o confirmation prompts.
            local_waiting: Regex::new(
                r"Permission required|Allow once|Allow always|Always allow|\(Y\)es.*\(N\)o",
            )
            .unwrap(),
            claude_thinking: Regex::new(r"(Thinking|Processing|Analyzing)\.\.\.|⠋|⠙|⠹|⠸").unwrap(),
            claude_ready: Regex::new(r"^>\s*$|Human:\s*$").unwrap(),
            claude_waiting: Regex::new(
                r"\[y/n\]|\[Y/n\]|\(yes/no\)|Press Enter|continue\?|proceed\?",
            )
            .unwrap(),
            codex_ready: Regex::new(r"codex>\s*$|>\s*$").unwrap(),
            codex_waiting: Regex::new(
                r"(?i)\b(?:approve|confirm)\b|\b(?:allow|proceed|continue)\b.*\?\s*$",
            )
            .unwrap(),
            generic_waiting: Regex::new(r"\[y/n\]|\[Y/n\]|\(yes/no\)|Press Enter").unwrap(),
            generic_error: Regex::new(
                r"(?i)^error[:\[]|^failed[:\[]|^panic!|^[Ff]atal:|command not found",
            )
                .unwrap(),

            nctx_re: Regex::new(r"\bn_ctx\s*=\s*(\d+)").unwrap(),

            // "$0.052" or "~$1.23" or "$12"
            cost_re: Regex::new(r"~?\$\s*(\d+(?:\.\d+)?)").unwrap(),
            // "12,345 input" | "input: 12,345" | "12.3k in" | "in: 12k"
            tokens_in_re: Regex::new(
                r"(?ix)
                (?:
                    (\d[\d,]*(?:\.\d+)?)\s*([k])?\s*(?:input|in(?:put)?\b)
                    |
                    (?:input|in(?:put)?\b)\s*:?\s*(\d[\d,]*(?:\.\d+)?)\s*([k])?
                )",
            )
            .unwrap(),
            // "3,456 output" | "output: 3,456" | "3.4k out"
            tokens_out_re: Regex::new(
                r"(?ix)
                (?:
                    (\d[\d,]*(?:\.\d+)?)\s*([k])?\s*(?:output|out(?:put)?\b)
                    |
                    (?:output|out(?:put)?\b)\s*:?\s*(\d[\d,]*(?:\.\d+)?)\s*([k])?
                )",
            )
            .unwrap(),
            // "18k tokens" | "18,456 tokens" | "tokens: 18456"
            tokens_total_re: Regex::new(
                r"(?i)(?:tokens?[:\s]+)?(\d[\d,]*(?:\.\d+)?)\s*([kK])?\s*tok(?:ens?)?",
            )
            .unwrap(),
        }
    }

    pub fn infer_state(&self, line: &str, base: BaseKind) -> Option<SessionState> {
        if self.generic_waiting.is_match(line) {
            return Some(SessionState::Waiting);
        }
        // Generic error detection only for shell/custom sessions — agents
        // report ERROR via process exit, JSONL records, or IPC.
        if matches!(base, BaseKind::Other) && self.generic_error.is_match(line) {
            return Some(SessionState::Error);
        }
        match base {
            BaseKind::Claude => {
                if self.claude_thinking.is_match(line) {
                    return Some(SessionState::Thinking);
                }
                if self.claude_waiting.is_match(line) {
                    return Some(SessionState::Waiting);
                }
                if self.claude_ready.is_match(line) {
                    return Some(SessionState::Ready);
                }
                if !line.trim().is_empty() {
                    return Some(SessionState::Running);
                }
            }
            BaseKind::Codex => {
                if self.codex_waiting.is_match(line) {
                    return Some(SessionState::Waiting);
                }
                if self.codex_ready.is_match(line) {
                    return Some(SessionState::Ready);
                }
                if !line.trim().is_empty() {
                    return Some(SessionState::Running);
                }
            }
            BaseKind::LocalAgent => {
                if self.local_waiting.is_match(line) {
                    return Some(SessionState::Waiting);
                }
                if self.local_thinking.is_match(line) {
                    return Some(SessionState::Thinking);
                }
                if self.local_ready.is_match(line) {
                    return Some(SessionState::Ready);
                }
                if !line.trim().is_empty() {
                    return Some(SessionState::Running);
                }
            }
            BaseKind::Other => {
                if self.shell_ready.is_match(line) {
                    return Some(SessionState::Ready);
                }
                if !line.trim().is_empty() {
                    return Some(SessionState::Running);
                }
            }
        }
        None
    }

    /// llama-cli / llama-server print "n_ctx = N" during model load; that's
    /// the context window the model was actually loaded with.
    pub fn parse_context_max(&self, line: &str) -> Option<u64> {
        self.nctx_re
            .captures(line)?
            .get(1)?
            .as_str()
            .parse::<u64>()
            .ok()
    }

    /// Scan an entire screen's text for token/cost statistics.
    /// Called on each tick for Claude/Codex sessions.
    pub fn parse_screen_stats(&self, text: &str) -> Option<TokenStats> {
        let cost = self
            .cost_re
            .captures(text)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<f64>().ok());

        let input = self
            .tokens_in_re
            .captures(text)
            .and_then(parse_token_capture);

        let output = self
            .tokens_out_re
            .captures(text)
            .and_then(parse_token_capture);

        // Fall back to total token count if input/output not found separately
        let total = if input.is_none() && output.is_none() {
            self.tokens_total_re
                .captures(text)
                .and_then(|c| parse_k_num(c.get(1)?.as_str(), c.get(2).map(|m| m.as_str())))
        } else {
            None
        };

        let (input_tokens, output_tokens) = match (input, output, total) {
            (Some(i), Some(o), _) => (i, o),
            (Some(i), None, _) => (i, 0),
            (None, Some(o), _) => (0, o),
            (None, None, Some(t)) => (t, 0),
            _ => {
                return cost.map(|c| TokenStats {
                    total_cost_usd: c,
                    ..Default::default()
                })
            }
        };

        let total_cost_usd = cost.unwrap_or_else(|| estimate_cost(input_tokens, output_tokens));

        if total_cost_usd == 0.0 && input_tokens == 0 && output_tokens == 0 {
            return None;
        }

        Some(TokenStats {
            input_tokens,
            output_tokens,
            total_cost_usd,
            context_tokens: 0,
        })
    }

    /// Line-based token parsing kept as a secondary signal for non-TUI output.
    pub fn parse_tokens(&self, line: &str) -> Option<TokenStats> {
        self.parse_screen_stats(line)
    }
}

fn parse_k_num(digits: &str, k: Option<&str>) -> Option<u64> {
    let clean = digits.replace(',', "");
    let n: f64 = clean.parse().ok()?;
    let mult = if k.is_some() { 1000.0 } else { 1.0 };
    Some((n * mult) as u64)
}

fn parse_token_capture(c: regex::Captures<'_>) -> Option<u64> {
    if let Some(n) = c.get(1) {
        return parse_k_num(n.as_str(), c.get(2).map(|m| m.as_str()));
    }
    parse_k_num(c.get(3)?.as_str(), c.get(4).map(|m| m.as_str()))
}

fn estimate_cost(input: u64, output: u64) -> f64 {
    const INPUT_PER_MTK: f64 = 3.0; // $3 / 1M input tokens
    const OUTPUT_PER_MTK: f64 = 15.0; // $15 / 1M output tokens
    (input as f64 / 1_000_000.0) * INPUT_PER_MTK + (output as f64 / 1_000_000.0) * OUTPUT_PER_MTK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_question_text_is_not_waiting() {
        let matcher = PatternMatcher::new();

        assert_eq!(
            matcher.infer_state("looks like for codex if i type ?", BaseKind::Codex),
            Some(SessionState::Running)
        );
    }

    #[test]
    fn codex_approval_prompts_are_waiting() {
        let matcher = PatternMatcher::new();

        for line in [
            "Approve?",
            "confirm",
            "Allow command?",
            "Do you want to continue?",
        ] {
            assert_eq!(
                matcher.infer_state(line, BaseKind::Codex),
                Some(SessionState::Waiting),
                "{line}"
            );
        }
    }

    #[test]
    fn shell_prompts_are_ready_and_non_empty_lines_are_running() {
        let matcher = PatternMatcher::new();

        assert_eq!(
            matcher.infer_state("user@host ~/repo $ ", BaseKind::Other),
            Some(SessionState::Ready)
        );
        assert_eq!(
            matcher.infer_state("building project", BaseKind::Other),
            Some(SessionState::Running)
        );
        assert_eq!(matcher.infer_state("", BaseKind::Other), None);
    }

    #[test]
    fn generic_waiting_and_error_take_precedence_for_all_session_kinds() {
        let matcher = PatternMatcher::new();

        // Generic waiting applies to all kinds
        for base in [BaseKind::Claude, BaseKind::Codex, BaseKind::Other] {
            assert_eq!(
                matcher.infer_state("Press Enter to continue", base),
                Some(SessionState::Waiting)
            );
        }

        // Generic error detection only applies to Other (shell/custom)
        assert_eq!(
            matcher.infer_state("fatal: command not found", BaseKind::Other),
            Some(SessionState::Error)
        );

        // Agents do NOT trigger Error from screen-scraped "error" text
        assert_ne!(
            matcher.infer_state("fatal: command not found", BaseKind::Claude),
            Some(SessionState::Error)
        );
        assert_ne!(
            matcher.infer_state("fatal: command not found", BaseKind::Codex),
            Some(SessionState::Error)
        );

        // Codex discussing an error file is just Running, not Error
        assert_eq!(
            matcher.infer_state(
                "fixed the error in ucp_tag_send.c",
                BaseKind::Codex
            ),
            Some(SessionState::Running)
        );

        // Shell session: anchored "Error:" at start of line -> Error
        assert_eq!(
            matcher.infer_state("Error: connection refused", BaseKind::Other),
            Some(SessionState::Error)
        );

        // Shell session: bare "error" mid-line does NOT match
        assert_ne!(
            matcher.infer_state("no error here", BaseKind::Other),
            Some(SessionState::Error)
        );
    }

    #[test]
    fn claude_specific_prompts_map_to_expected_states() {
        let matcher = PatternMatcher::new();

        assert_eq!(
            matcher.infer_state("Thinking...", BaseKind::Claude),
            Some(SessionState::Thinking)
        );
        assert_eq!(
            matcher.infer_state("Human:", BaseKind::Claude),
            Some(SessionState::Ready)
        );
        assert_eq!(
            matcher.infer_state("shall I proceed?", BaseKind::Claude),
            Some(SessionState::Waiting)
        );
    }

    #[test]
    fn local_agent_permission_dialogs_are_waiting_even_with_spinner() {
        let matcher = PatternMatcher::new();

        for line in [
            "△ Permission required",
            "  Allow once   Allow always   Reject",
            "⠙ △ Permission required", // spinner remnant must not win
            "Add foo.py to the chat? (Y)es/(N)o [Yes]:",
        ] {
            assert_eq!(
                matcher.infer_state(line, BaseKind::LocalAgent),
                Some(SessionState::Waiting),
                "{line}"
            );
        }

        assert_eq!(
            matcher.infer_state("⠙ working... esc to interrupt", BaseKind::LocalAgent),
            Some(SessionState::Thinking)
        );
    }

    #[test]
    fn parse_context_max_reads_llama_nctx_line() {
        let matcher = PatternMatcher::new();
        assert_eq!(
            matcher.parse_context_max("llama_context: n_ctx         = 8192"),
            Some(8192)
        );
        assert_eq!(matcher.parse_context_max("loading model..."), None);
    }

    #[test]
    fn parse_stats_extracts_cost_input_and_output_tokens() {
        let matcher = PatternMatcher::new();
        let stats = matcher
            .parse_screen_stats("usage: 12.3k input, 456 output, ~$0.052")
            .unwrap();

        assert_eq!(stats.input_tokens, 12_300);
        assert_eq!(stats.output_tokens, 456);
        assert_eq!(stats.total_cost_usd, 0.052);
    }

    #[test]
    fn parse_stats_extracts_label_first_input_and_output_tokens() {
        let matcher = PatternMatcher::new();
        let stats = matcher
            .parse_screen_stats("usage: input: 12.3k, output: 456, ~$0.052")
            .unwrap();

        assert_eq!(stats.input_tokens, 12_300);
        assert_eq!(stats.output_tokens, 456);
        assert_eq!(stats.total_cost_usd, 0.052);
    }

    #[test]
    fn parse_stats_uses_total_tokens_when_split_counts_are_absent() {
        let matcher = PatternMatcher::new();
        let stats = matcher.parse_screen_stats("18.5k tokens").unwrap();

        assert_eq!(stats.input_tokens, 18_500);
        assert_eq!(stats.output_tokens, 0);
        assert!(stats.total_cost_usd > 0.0);
    }

    #[test]
    fn parse_stats_estimates_cost_when_no_explicit_cost_exists() {
        let matcher = PatternMatcher::new();
        let stats = matcher
            .parse_screen_stats("1,000 input and 2k output")
            .unwrap();

        assert_eq!(stats.input_tokens, 1_000);
        assert_eq!(stats.output_tokens, 2_000);
        assert!((stats.total_cost_usd - 0.033).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_stats_returns_cost_only_when_tokens_are_missing() {
        let matcher = PatternMatcher::new();
        let stats = matcher.parse_screen_stats("spent $12").unwrap();

        assert_eq!(stats.input_tokens, 0);
        assert_eq!(stats.output_tokens, 0);
        assert_eq!(stats.total_cost_usd, 12.0);
    }
}
