use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Diagnostic, Pass, PassContext};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedCompression {
    pub model: String,
    pub output: String,
}

/// The `passes` portion of `cybersin.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionLock {
    #[serde(default)]
    pub compress: BTreeMap<String, LockedCompression>,
}

pub trait CompressionProvider: Send + Sync {
    fn model(&self) -> &str;
    fn compress(&self, input: &str) -> Result<String, CompressError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressError(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressMode {
    Update,
    Frozen,
}

pub fn input_hash(input: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"cybersin.compress.v1\0");
    hash.update(input.as_bytes());
    format!("{:x}", hash.finalize())
}

#[derive(Clone)]
pub struct Compress {
    provider: Arc<dyn CompressionProvider>,
    lock: Arc<Mutex<CompressionLock>>,
    mode: CompressMode,
}

impl Compress {
    pub fn new(
        provider: Arc<dyn CompressionProvider>,
        lock: Arc<Mutex<CompressionLock>>,
        mode: CompressMode,
    ) -> Self {
        Self {
            provider,
            lock,
            mode,
        }
    }

    pub fn lock(&self) -> Arc<Mutex<CompressionLock>> {
        Arc::clone(&self.lock)
    }
}

impl Pass for Compress {
    fn name(&self) -> &'static str {
        "compress"
    }

    fn run(&self, ctx: &mut PassContext) {
        let mut sections = ctx.ir.sections.clone();
        for section in &mut sections {
            if section.body.is_empty() || section.dedup_ref.is_some() {
                continue;
            }
            let key = input_hash(&section.body);
            let pinned = self.lock.lock().unwrap().compress.get(&key).cloned();
            let output = if let Some(pinned) = pinned {
                pinned.output
            } else {
                if self.mode == CompressMode::Frozen {
                    ctx.push(Diagnostic::error(
                        self.name(),
                        format!(
                            "frozen build: compression input {key} is not pinned in cybersin.lock and would require a network call"
                        ),
                    ));
                    return;
                }
                match self.provider.compress(&section.body) {
                    Ok(output) => {
                        if output.split_whitespace().count()
                            >= section.body.split_whitespace().count()
                        {
                            ctx.push(Diagnostic::error(
                                self.name(),
                                format!(
                                    "provider did not reduce token count for section `{}`",
                                    section.id
                                ),
                            ));
                            return;
                        }
                        self.lock.lock().unwrap().compress.insert(
                            key,
                            LockedCompression {
                                model: self.provider.model().to_string(),
                                output: output.clone(),
                            },
                        );
                        output
                    }
                    Err(error) => {
                        ctx.push(Diagnostic::error(
                            self.name(),
                            format!("compression provider failed: {}", error.0),
                        ));
                        return;
                    }
                }
            };
            section.body = output;
        }
        ctx.ir.sections = sections;
    }
}
