use regex::Regex;
use crate::session::{SessionKind, SessionState, TokenStats};

pub struct PatternMatcher {
    // Shell/generic ready prompts
    shell_ready: Regex,
    // Claude-specific patterns
    claude_thinking: Regex,
    claude_ready: Regex,
    claude_waiting: Regex,
    claude_tokens: Regex,
    // Codex patterns
    codex_ready: Regex,
    codex_waiting: Regex,
    // Generic waiting/error
    generic_waiting: Regex,
    generic_error: Regex,
}

impl PatternMatcher {
    pub fn new() -> Self {
        Self {
            shell_ready:     Regex::new(r"[\$#>]\s*$").unwrap(),
            claude_thinking: Regex::new(r"(Thinking|Processing|Analyzing)\.\.\.|⠋|⠙|⠹|⠸").unwrap(),
            claude_ready:    Regex::new(r"^>\s*$|Human:\s*$|^\s*$").unwrap(),
            claude_waiting:  Regex::new(r"\?\s*$|\[y/n\]|\[Y/n\]|\(yes/no\)|Press Enter|continue\?").unwrap(),
            claude_tokens:   Regex::new(r"(?i)tokens?[:\s]+(\d+)\s*/\s*(\d+)|input[:\s]+(\d+).*output[:\s]+(\d+)").unwrap(),
            codex_ready:     Regex::new(r"codex>\s*$|>\s*$").unwrap(),
            codex_waiting:   Regex::new(r"\?\s*$|Approve\?|confirm").unwrap(),
            generic_waiting: Regex::new(r"\?\s*$|\[y/n\]|\[Y/n\]|Press Enter").unwrap(),
            generic_error:   Regex::new(r"(?i)error:|failed:|panic!|fatal:|command not found").unwrap(),
        }
    }

    /// Infer new session state from a line of output.
    /// Returns None if the line doesn't change state.
    pub fn infer_state(&self, line: &str, kind: &SessionKind) -> Option<SessionState> {
        // Waiting is highest priority — agent is blocked on us
        if self.generic_waiting.is_match(line) {
            return Some(SessionState::Waiting);
        }

        // Error detection
        if self.generic_error.is_match(line) {
            return Some(SessionState::Error);
        }

        match kind {
            SessionKind::Claude => {
                if self.claude_thinking.is_match(line) {
                    return Some(SessionState::Thinking);
                }
                if self.claude_waiting.is_match(line) {
                    return Some(SessionState::Waiting);
                }
                if self.claude_ready.is_match(line) {
                    return Some(SessionState::Ready);
                }
                // Active output = running
                if !line.trim().is_empty() {
                    return Some(SessionState::Running);
                }
            }
            SessionKind::Codex => {
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
            SessionKind::Shell | SessionKind::Custom(_) => {
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

    /// Try to parse token usage from a line.
    /// Returns Some(TokenStats) if found.
    pub fn parse_tokens(&self, line: &str) -> Option<TokenStats> {
        if let Some(caps) = self.claude_tokens.captures(line) {
            // Try first format: "tokens: 123 / 456"
            if let (Some(inp), Some(out)) = (caps.get(1), caps.get(2)) {
                let input: u64 = inp.as_str().parse().ok()?;
                let output: u64 = out.as_str().parse().ok()?;
                return Some(TokenStats {
                    input_tokens: input,
                    output_tokens: output,
                    total_cost_usd: estimate_cost(input, output),
                });
            }
            // Try second format: "input: 123 output: 456"
            if let (Some(inp), Some(out)) = (caps.get(3), caps.get(4)) {
                let input: u64 = inp.as_str().parse().ok()?;
                let output: u64 = out.as_str().parse().ok()?;
                return Some(TokenStats {
                    input_tokens: input,
                    output_tokens: output,
                    total_cost_usd: estimate_cost(input, output),
                });
            }
        }
        None
    }
}

/// Rough Claude Sonnet pricing estimate (update as needed)
fn estimate_cost(input: u64, output: u64) -> f64 {
    const INPUT_PER_MTK: f64  = 3.0;   // $3 per 1M input tokens
    const OUTPUT_PER_MTK: f64 = 15.0;  // $15 per 1M output tokens
    (input as f64 / 1_000_000.0) * INPUT_PER_MTK
        + (output as f64 / 1_000_000.0) * OUTPUT_PER_MTK
}
