use serde::{Deserialize, Serialize};

pub struct Limit {
    pub memory: Option<u64>,
    pub time_limit: Option<u64>,
    pub walltime_limit: Option<u64>,
}

#[derive(Serialize, Debug, Deserialize)]
pub enum RunStatus {
    #[serde(rename = "success")]
    Success,

    #[serde(rename = "tle")]
    TimeLimitExceeded,

    #[serde(rename = "system_error")]
    SystemError(String),

    #[serde(rename = "runtime_error")]
    RuntimeError(String),
}

#[derive(Serialize, Debug)]
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub runtime: u128,
    pub memory_usage: i64,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
}

impl RunOutput {
    pub fn error(reason: String, stderr: Option<Vec<u8>>, stdout: Option<Vec<u8>>) -> Self {
        Self {
            stdout: stdout.unwrap_or(Vec::new()),
            stderr: stderr.unwrap_or(Vec::new()),
            runtime: 0,
            memory_usage: 0,
            status: RunStatus::SystemError(reason),
            exit_code: None,
        }
    }
}
