#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub pages_scanned: u64,
    pub anomalies: u64,
    pub bloom_load_factor: f64,
    pub elapsed_ms: u64,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.anomalies == 0
    }
}
