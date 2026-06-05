use regex::Regex;
use crate::session::{SessionKind, SessionState, TokenStats};

pub struct PatternMatcher {
    // Shell/generic ready prompts
    shell_ready: Regex,
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
    // Token / cost extraction (applied to full screen text)
    cost_re: Regex,
    tokens_in_re: Regex,
    tokens_out_re: Regex,
    tokens_total_re: Regex,
}

impl PatternMatcher {
    pub fn new() -> Self {
        Self {
            shell_ready:     Regex::new(r"[\$#%>]\s*$").unwrap(),
            claude_thinking: Regex::new(r"(Thinking|Processing|Analyzing)\.\.\.|⠋|⠙|⠹|⠸").unwrap(),
            claude_ready:    Regex::new(r"^>\s*$|Human:\s*$|^\s*$").unwrap(),
            claude_waiting:  Regex::new(r"\?\s*$|\[y/n\]|\[Y/n\]|\(yes/no\)|Press Enter|continue\?").unwrap(),
            codex_ready:     Regex::new(r"codex>\s*$|>\s*$").unwrap(),
            codex_waiting:   Regex::new(r"\?\s*$|Approve\?|confirm").unwrap(),
            generic_waiting: Regex::new(r"\?\s*$|\[y/n\]|\[Y/n\]|Press Enter").unwrap(),
            generic_error:   Regex::new(r"(?i)error:|failed:|panic!|fatal:|command not found").unwrap(),

            // "$0.052" or "~$1.23" or "$12"
            cost_re:         Regex::new(r"~?\$\s*(\d+(?:\.\d+)?)").unwrap(),
            // "12,345 input" | "input: 12,345" | "12.3k in" | "in: 12k"
            tokens_in_re:    Regex::new(
                r"(?i)(\d[\d,]*(?:\.\d+)?)\s*([kK])?\s*(?:input|in(?:put)?\b)"
            ).unwrap(),
            // "3,456 output" | "output: 3,456" | "3.4k out"
            tokens_out_re:   Regex::new(
                r"(?i)(\d[\d,]*(?:\.\d+)?)\s*([kK])?\s*(?:output|out(?:put)?\b)"
            ).unwrap(),
            // "18k tokens" | "18,456 tokens" | "tokens: 18456"
            tokens_total_re: Regex::new(
                r"(?i)(?:tokens?[:\s]+)?(\d[\d,]*(?:\.\d+)?)\s*([kK])?\s*tok(?:ens?)?"
            ).unwrap(),
        }
    }

    pub fn infer_state(&self, line: &str, kind: &SessionKind) -> Option<SessionState> {
        if self.generic_waiting.is_match(line) {
            return Some(SessionState::Waiting);
        }
        if self.generic_error.is_match(line) {
            return Some(SessionState::Error);
        }
        match kind {
            SessionKind::Claude => {
                if self.claude_thinking.is_match(line) { return Some(SessionState::Thinking); }
                if self.claude_waiting.is_match(line)  { return Some(SessionState::Waiting); }
                if self.claude_ready.is_match(line)    { return Some(SessionState::Ready); }
                if !line.trim().is_empty()             { return Some(SessionState::Running); }
            }
            SessionKind::Codex => {
                if self.codex_waiting.is_match(line) { return Some(SessionState::Waiting); }
                if self.codex_ready.is_match(line)   { return Some(SessionState::Ready); }
                if !line.trim().is_empty()            { return Some(SessionState::Running); }
            }
            SessionKind::Shell | SessionKind::Custom(_) => {
                if self.shell_ready.is_match(line) { return Some(SessionState::Ready); }
                if !line.trim().is_empty()         { return Some(SessionState::Running); }
            }
        }
        None
    }

    /// Scan an entire screen's text for token/cost statistics.
    /// Called on each tick for Claude/Codex sessions.
    pub fn parse_screen_stats(&self, text: &str) -> Option<TokenStats> {
        let cost = self.cost_re.captures(text)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<f64>().ok());

        let input = self.tokens_in_re.captures(text)
            .and_then(|c| parse_k_num(c.get(1)?.as_str(), c.get(2).map(|m| m.as_str())));

        let output = self.tokens_out_re.captures(text)
            .and_then(|c| parse_k_num(c.get(1)?.as_str(), c.get(2).map(|m| m.as_str())));

        // Fall back to total token count if input/output not found separately
        let total = if input.is_none() && output.is_none() {
            self.tokens_total_re.captures(text)
                .and_then(|c| parse_k_num(c.get(1)?.as_str(), c.get(2).map(|m| m.as_str())))
        } else {
            None
        };

        let (input_tokens, output_tokens) = match (input, output, total) {
            (Some(i), Some(o), _) => (i, o),
            (Some(i), None, _)    => (i, 0),
            (None, Some(o), _)    => (0, o),
            (None, None, Some(t)) => (t, 0),
            _                     => return cost.map(|c| TokenStats { total_cost_usd: c, ..Default::default() }),
        };

        let total_cost_usd = cost.unwrap_or_else(|| estimate_cost(input_tokens, output_tokens));

        if total_cost_usd == 0.0 && input_tokens == 0 && output_tokens == 0 {
            return None;
        }

        Some(TokenStats { input_tokens, output_tokens, total_cost_usd, context_tokens: 0 })
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

fn estimate_cost(input: u64, output: u64) -> f64 {
    const INPUT_PER_MTK:  f64 = 3.0;   // $3 / 1M input tokens
    const OUTPUT_PER_MTK: f64 = 15.0;  // $15 / 1M output tokens
    (input  as f64 / 1_000_000.0) * INPUT_PER_MTK
        + (output as f64 / 1_000_000.0) * OUTPUT_PER_MTK
}
