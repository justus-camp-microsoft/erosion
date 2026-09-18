use serde::{Deserialize, Serialize};

pub const METRIC_VERSION: &str = "erosion-v3";
pub const COMPLEXITY_THRESHOLD: u64 = 10;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FunctionMetrics {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub complexity: u64,
    pub source_lines: usize,
}

impl FunctionMetrics {
    pub fn mass(&self) -> f64 {
        self.complexity as f64 * (self.source_lines as f64).sqrt()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ParseDiagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TestRegion {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TestFilteredAnalysis {
    pub source_lines: usize,
    pub functions: Vec<FunctionMetrics>,
    pub regions: Vec<TestRegion>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FileAnalysis {
    pub physical_lines: usize,
    pub source_lines: usize,
    pub functions: Vec<FunctionMetrics>,
    pub diagnostics: Vec<ParseDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub without_tests: Option<TestFilteredAnalysis>,
}

impl FileAnalysis {
    pub fn parsed(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

#[derive(Default)]
pub struct Sum {
    sum: f64,
    correction: f64,
}

impl Sum {
    pub fn add(&mut self, value: f64) {
        let next = self.sum + value;
        self.correction += if self.sum.abs() >= value.abs() {
            (self.sum - next) + value
        } else {
            (value - next) + self.sum
        };
        self.sum = next;
    }

    pub fn total(&self) -> f64 {
        self.sum + self.correction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compensated_sum_keeps_small_terms() {
        let mut sum = Sum::default();
        for value in [1e16, 1.0, -1e16] {
            sum.add(value);
        }
        assert_eq!(sum.total(), 1.0);
    }
}
