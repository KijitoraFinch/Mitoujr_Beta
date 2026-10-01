use crate::engine::Engine;
use crate::model::{RegionSelector, Relation};
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct TestReport {
    pub passed: bool,
    pub complete: bool,
    pub selectors: usize,
    pub comparison_calls: usize,
    pub failures: Vec<TestFailure>,
}

#[derive(Debug, Serialize)]
pub struct TestFailure {
    pub law: String,
    pub inputs: Vec<usize>,
    pub message: String,
}

/// Test observable consistency and comparison laws under stable external state.
/// Extension-owned caches remain active. These samples do not prove that an
/// extension's dependency tracking or cache invalidation is complete.
pub async fn test_regions(
    engine: &mut Engine<'_>,
    selectors: &[RegionSelector],
) -> Result<TestReport> {
    ensure!(
        !selectors.is_empty(),
        "extension tests require at least one selector"
    );
    let mut report = TestReport {
        passed: true,
        complete: true,
        selectors: selectors.len(),
        comparison_calls: 0,
        failures: Vec::new(),
    };
    let mut regions = Vec::new();
    for (index, selector) in selectors.iter().enumerate() {
        match engine.resolve(selector).await {
            Ok(region) => {
                match engine.resolve(selector).await {
                    Ok(repeated) => {
                        report.comparison_calls += 1;
                        match engine.compare(&region, &repeated).await {
                            Ok(Relation::Equal) => {}
                            Ok(_) => report.fail(
                                "resolution_determinism",
                                vec![index],
                                "repeated resolution returned a different region",
                            ),
                            Err(error) => {
                                report.complete = false;
                                report.fail(
                                    "resolution_determinism",
                                    vec![index],
                                    format!("{error:#}"),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        report.complete = false;
                        report.fail("resolution_determinism", vec![index], format!("{error:#}"))
                    }
                }
                regions.push(Some(region));
            }
            Err(error) => {
                report.complete = false;
                report.fail("resolution", vec![index], format!("{error:#}"));
                regions.push(None);
            }
        }
    }
    let count = selectors.len();
    let mut comparisons = vec![vec![None; count]; count];
    for a in 0..count {
        for b in 0..count {
            let (Some(left), Some(right)) = (&regions[a], &regions[b]) else {
                continue;
            };
            report.comparison_calls += 1;
            match engine.compare(left, right).await {
                Ok(relation) => {
                    comparisons[a][b] = Some(relation);
                    report.comparison_calls += 1;
                    match engine.compare(left, right).await {
                        Ok(repeated) if repeated == relation => {}
                        Ok(_) => report.fail(
                            "comparison_determinism",
                            vec![a, b],
                            "repeated comparison returned a different relation",
                        ),
                        Err(error) => {
                            report.complete = false;
                            report.fail("comparison_determinism", vec![a, b], format!("{error:#}"))
                        }
                    }
                }
                Err(error) => {
                    report.complete = false;
                    report.fail("comparison", vec![a, b], format!("{error:#}"));
                }
            }
        }
    }
    let includes = |relation| matches!(relation, Some(Relation::Equal | Relation::Contains));
    for a in 0..count {
        tokio::task::yield_now().await;
        if comparisons[a][a].is_some_and(|relation| relation != Relation::Equal) {
            report.fail("reflexivity", vec![a], "a region must equal itself");
        }
        for b in 0..count {
            let (Some(ab), Some(ba)) = (comparisons[a][b], comparisons[b][a]) else {
                continue;
            };
            let inverse = match ab {
                Relation::Equal => Relation::Equal,
                Relation::Contains => Relation::Within,
                Relation::Within => Relation::Contains,
                Relation::Incomparable => Relation::Incomparable,
            };
            if ba != inverse {
                report.fail(
                    "symmetry",
                    vec![a, b],
                    "reversing the operands did not reverse the relation",
                );
            }
            if (ab == Relation::Equal) != (includes(Some(ab)) && includes(Some(ba))) {
                report.fail(
                    "antisymmetry",
                    vec![a, b],
                    "mutual containment must mean equality",
                );
            }
            for (c, (&ac, &bc)) in comparisons[a].iter().zip(&comparisons[b]).enumerate() {
                if includes(Some(ab)) && includes(bc) && ac.is_some() && !includes(ac) {
                    report.fail(
                        "transitivity",
                        vec![a, b, c],
                        "a contains b and b contains c, but a does not contain c",
                    );
                }
            }
        }
    }
    Ok(report)
}

impl TestReport {
    fn fail(&mut self, law: &str, inputs: Vec<usize>, message: impl Into<String>) {
        self.passed = false;
        self.failures.push(TestFailure {
            law: law.into(),
            inputs,
            message: message.into(),
        });
    }
}
