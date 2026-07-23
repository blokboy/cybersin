//! Cost core: rollup dimension and result row (spec §8.5's `cybersin cost
//! --by session|agent|model|tool|day`).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The grouping dimension for a cost rollup — one variant per
/// `cybersin cost --by <dim>` value the spec names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostDimension {
    Session,
    Agent,
    Model,
    Tool,
    Day,
}

impl fmt::Display for CostDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CostDimension::Session => "session",
            CostDimension::Agent => "agent",
            CostDimension::Model => "model",
            CostDimension::Tool => "tool",
            CostDimension::Day => "day",
        })
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown cost dimension {0:?}; expected one of session|agent|model|tool|day")]
pub struct ParseCostDimensionError(String);

impl FromStr for CostDimension {
    type Err = ParseCostDimensionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session" => Ok(CostDimension::Session),
            "agent" => Ok(CostDimension::Agent),
            "model" => Ok(CostDimension::Model),
            "tool" => Ok(CostDimension::Tool),
            "day" => Ok(CostDimension::Day),
            other => Err(ParseCostDimensionError(other.to_string())),
        }
    }
}

/// One row of a cost rollup: total spend and token counts for one bucket
/// (a session id, agent name, model name, tool name, or `YYYY-MM-DD` day)
/// under the requested [`CostDimension`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRollupRow {
    /// The bucket key: a session id, agent name, model name, tool name, or
    /// ISO date, depending on which [`CostDimension`] produced this row.
    pub key: String,
    pub usd_cost: f64,
    pub span_count: u64,
    pub tokens_prompt: u64,
    pub tokens_completion: u64,
}
