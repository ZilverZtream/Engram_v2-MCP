#[derive(Debug, Clone)]
pub enum ImmuneDecision {
    Allow,
    Warn { message: String, confidence: f32 },
    Block { message: String, confidence: f32 },
}

#[derive(Clone)]
pub struct ImmuneEngine {
    pub warn_threshold: f32,
    pub block_threshold: f32,
}

impl Default for ImmuneEngine {
    fn default() -> Self {
        Self {
            warn_threshold: 0.01,
            block_threshold: 0.03,
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
