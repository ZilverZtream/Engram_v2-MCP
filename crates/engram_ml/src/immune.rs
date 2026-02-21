#[derive(Debug, Clone)]
pub enum ImmuneDecision {
    Allow,
    Warn { message: String, confidence: f32 },
    Block { message: String, confidence: f32 },
}

impl ImmuneDecision {
    /// Returns a severity label: "none", "low", "medium", "high", "critical".
    pub fn severity(&self) -> &'static str {
        match self {
            ImmuneDecision::Allow => "none",
            ImmuneDecision::Warn { confidence, .. } => {
                if *confidence >= 0.35 {
                    "medium"
                } else {
                    "low"
                }
            }
            ImmuneDecision::Block { confidence, .. } => {
                if *confidence >= 0.75 {
                    "critical"
                } else {
                    "high"
                }
            }
        }
    }

    /// Returns a short verdict label for structured output.
    pub fn verdict(&self) -> &'static str {
        match self {
            ImmuneDecision::Allow => "PASS",
            ImmuneDecision::Warn { .. } => "WARN",
            ImmuneDecision::Block { .. } => "BLOCK",
        }
    }

    /// Whether the caller should take action (true for Warn and Block).
    pub fn action_required(&self) -> bool {
        !matches!(self, ImmuneDecision::Allow)
    }
}

#[derive(Clone)]
pub struct ImmuneEngine {
    pub warn_threshold: f32,
    pub block_threshold: f32,
}

impl Default for ImmuneEngine {
    fn default() -> Self {
        Self {
            warn_threshold: 0.15,
            block_threshold: 0.45,
        }
    }
}

impl ImmuneEngine {
    pub fn new(warn_threshold: f32, block_threshold: f32) -> Self {
        Self {
            warn_threshold,
            block_threshold,
        }
    }

    /// Decide whether to allow/warn/block code generation based on similarity to reverted diffs.
    ///
    /// `similarity` should be normalized to [0, 1].
    pub fn decide(&self, similarity: f32, context: Option<&str>) -> ImmuneDecision {
        if similarity >= self.block_threshold {
            let mut msg =
                "This pattern strongly resembles a previously reverted change.".to_string();
            if let Some(c) = context {
                msg.push_str("\n\nClosest reverted snippet:\n");
                msg.push_str(c);
            }
            return ImmuneDecision::Block {
                message: msg,
                confidence: similarity,
            };
        }

        if similarity >= self.warn_threshold {
            let mut msg =
                "This pattern resembles code that was reverted before. Proceed carefully."
                    .to_string();
            if let Some(c) = context {
                msg.push_str("\n\nClosest reverted snippet:\n");
                msg.push_str(c);
            }
            return ImmuneDecision::Warn {
                message: msg,
                confidence: similarity,
            };
        }

        ImmuneDecision::Allow
    }
}
