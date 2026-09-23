use std::cmp::Ordering;

use crate::github::{RunCounts, RunStatus, Workflow};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilterExpression {
    clauses: Vec<Clause>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Clause {
    negated: bool,
    field: Option<String>,
    values: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortSpec {
    pub field: String,
    pub descending: bool,
}

impl FilterExpression {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let clauses = tokenize(expression)?
            .into_iter()
            .map(|token| {
                let (negated, token) = token
                    .strip_prefix('-')
                    .map_or((false, token.as_str()), |token| (true, token));
                if token.is_empty() {
                    return Err("filter contains an empty negated clause".to_owned());
                }
                let (field, value) = token
                    .split_once(':')
                    .map_or((None, token), |(field, value)| {
                        (Some(field.to_owned()), value)
                    });
                if value.is_empty() {
                    return Err(format!("filter clause '{token}' is missing a value"));
                }
                let values = value.split(',').map(str::to_owned).collect::<Vec<_>>();
                if let Some(keyword) = values.iter().find(|value| value.starts_with('@')) {
                    return Err(format!("filter keyword '{keyword}' is not supported"));
                }
                Ok(Clause {
                    negated,
                    field,
                    values,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { clauses })
    }

    pub fn matches(&self, workflow: &Workflow) -> bool {
        self.clauses.iter().all(|clause| {
            let matched = clause.matches(workflow);
            matched != clause.negated
        })
    }
}

impl Clause {
    fn matches(&self, workflow: &Workflow) -> bool {
        let Some(field) = self.field.as_deref() else {
            return self
                .values
                .iter()
                .any(|value| matches_general_text(workflow, value));
        };
        match normalized(field).as_str() {
            "has" => self
                .values
                .iter()
                .any(|name| workflow_value(workflow, name).is_some_and(|value| !value.is_empty())),
            "no" => self
                .values
                .iter()
                .any(|name| workflow_value(workflow, name).is_none_or(|value| value.is_empty())),
            "is" => self
                .values
                .iter()
                .any(|value| matches_status(workflow, value)),
            _ => workflow_value(workflow, field).is_some_and(|actual| {
                self.values
                    .iter()
                    .any(|expected| matches_value(&actual, expected))
            }),
        }
    }
}

impl SortSpec {
    pub fn parse(specification: &str) -> Result<Self, String> {
        let specification = specification.trim();
        if specification.is_empty() {
            return Err("sort is missing a field".to_owned());
        }
        let (field, descending) = match specification.rsplit_once(':') {
            Some((field, direction)) if direction.eq_ignore_ascii_case("asc") => (field, false),
            Some((field, direction)) if direction.eq_ignore_ascii_case("desc") => (field, true),
            Some((_, direction)) => {
                return Err(format!(
                    "invalid sort direction '{direction}'; use asc or desc"
                ));
            }
            None => (specification, false),
        };
        let field = field.trim_matches(['\'', '"']).trim();
        if field.is_empty() {
            return Err("sort is missing a field".to_owned());
        }
        Ok(Self {
            field: field.to_owned(),
            descending,
        })
    }
}

pub fn select_workflows<'a>(
    workflows: &'a [Workflow],
    filter: Option<&FilterExpression>,
    sort: Option<&SortSpec>,
) -> Vec<&'a Workflow> {
    let mut selected = workflows
        .iter()
        .filter(|workflow| filter.is_none_or(|filter| filter.matches(workflow)))
        .collect::<Vec<_>>();
    if let Some(sort) = sort {
        selected.sort_by(|left, right| {
            compare_optional(
                workflow_value(left, &sort.field),
                workflow_value(right, &sort.field),
                sort.descending,
            )
        });
    }
    selected
}

fn workflow_value(workflow: &Workflow, field: &str) -> Option<String> {
    let field = normalized(field);
    match field.as_str() {
        "name" | "workflow" => Some(workflow.name.clone()),
        "status" | "state" => Some(status_value(workflow).to_owned()),
        "24h" | "24hrate" => Some(metric_value(workflow.run_metrics.last_24_hours, "rate")),
        "7d" | "7drate" => Some(metric_value(workflow.run_metrics.last_7_days, "rate")),
        "14d" | "14drate" => Some(metric_value(workflow.run_metrics.last_14_days, "rate")),
        _ => metric_field(workflow, &field),
    }
}

fn metric_field(workflow: &Workflow, field: &str) -> Option<String> {
    for (prefix, counts) in [
        ("24h", workflow.run_metrics.last_24_hours),
        ("7d", workflow.run_metrics.last_7_days),
        ("14d", workflow.run_metrics.last_14_days),
    ] {
        if let Some(component) = field.strip_prefix(prefix) {
            return Some(metric_value(counts, component));
        }
    }
    None
}

fn metric_value(counts: RunCounts, component: &str) -> String {
    match component {
        "pass" | "passed" => counts.passed.to_string(),
        "fail" | "failed" => counts.failed.to_string(),
        "total" => counts.total.to_string(),
        "rate" | "percent" | "percentage" | "" => counts.pass_percentage().to_string(),
        _ => String::new(),
    }
}

fn status_value(workflow: &Workflow) -> &'static str {
    if workflow.is_in_progress {
        "in_progress"
    } else {
        match workflow.run_status {
            RunStatus::Success => "success",
            RunStatus::Failure => "failure",
            RunStatus::Other => "other",
        }
    }
}

fn matches_status(workflow: &Workflow, expected: &str) -> bool {
    matches_value(status_value(workflow), expected)
}

fn matches_general_text(workflow: &Workflow, expected: &str) -> bool {
    if expected.contains('*') {
        matches_value(&workflow.name, expected)
    } else {
        workflow
            .name
            .split(|character: char| !character.is_alphanumeric())
            .any(|word| word.to_lowercase().starts_with(&expected.to_lowercase()))
    }
}

fn matches_value(actual: &str, expected: &str) -> bool {
    if let Some((operator, expected)) = comparison(expected) {
        return compare_scalar(actual, expected).is_some_and(|ordering| match operator {
            ">" => ordering.is_gt(),
            ">=" => ordering.is_ge(),
            "<" => ordering.is_lt(),
            "<=" => ordering.is_le(),
            _ => false,
        });
    }
    if let Some((start, end)) = expected.split_once("..") {
        let after_start =
            start == "*" || compare_scalar(actual, start).is_some_and(Ordering::is_ge);
        let before_end = end == "*" || compare_scalar(actual, end).is_some_and(Ordering::is_le);
        return after_start && before_end;
    }
    wildcard_match(actual, expected)
}

fn comparison(expected: &str) -> Option<(&str, &str)> {
    [">=", "<=", ">", "<"].into_iter().find_map(|operator| {
        expected
            .strip_prefix(operator)
            .map(|value| (operator, value))
    })
}

fn compare_scalar(left: &str, right: &str) -> Option<Ordering> {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right),
        _ => Some(left.to_lowercase().cmp(&right.to_lowercase())),
    }
}

fn compare_optional(left: Option<String>, right: Option<String>, descending: bool) -> Ordering {
    match (
        left.filter(|value| !value.is_empty()),
        right.filter(|value| !value.is_empty()),
    ) {
        (Some(left), Some(right)) => {
            let ordering = compare_scalar(&left, &right).unwrap_or(Ordering::Equal);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn wildcard_match(actual: &str, expected: &str) -> bool {
    let actual = actual.to_lowercase();
    let expected = expected.to_lowercase();
    match (expected.starts_with('*'), expected.ends_with('*')) {
        (true, true) => actual.contains(expected.trim_matches('*')),
        (true, false) => actual.ends_with(expected.trim_start_matches('*')),
        (false, true) => actual.starts_with(expected.trim_end_matches('*')),
        (false, false) => actual == expected,
    }
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn tokenize(expression: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in expression.chars() {
        if escaped {
            token.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none() && character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(character);
        }
    }
    if quote.is_some() {
        return Err("filter contains an unterminated quote".to_owned());
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{RunMetrics, RunStatus};

    fn workflow(name: &str, status: RunStatus, rate: RunCounts) -> Workflow {
        Workflow {
            id: name.len() as u64,
            name: name.to_owned(),
            path: String::new(),
            state: "active".to_owned(),
            run_status: status,
            is_in_progress: false,
            run_metrics: RunMetrics {
                last_24_hours: rate,
                last_7_days: rate,
                last_14_days: rate,
            },
        }
    }

    #[test]
    fn filter_supports_github_style_clauses() {
        let workflow = workflow(
            "Linux Build",
            RunStatus::Success,
            RunCounts {
                passed: 8,
                failed: 2,
                total: 10,
            },
        );

        for expression in [
            "Linux",
            "name:\"Linux Build\"",
            "name:*Build",
            "status:success,failure",
            "-status:failure",
            "has:24h.total",
            "24h.rate:>=80",
            "24h.pass:5..10",
        ] {
            assert!(
                FilterExpression::parse(expression)
                    .unwrap()
                    .matches(&workflow)
            );
        }
    }

    #[test]
    fn select_workflows_sorts_numeric_fields() {
        let workflows = vec![
            workflow(
                "Low",
                RunStatus::Failure,
                RunCounts {
                    passed: 1,
                    failed: 9,
                    total: 10,
                },
            ),
            workflow(
                "High",
                RunStatus::Success,
                RunCounts {
                    passed: 9,
                    failed: 1,
                    total: 10,
                },
            ),
        ];
        let sort = SortSpec::parse("24h.rate:desc").unwrap();

        let selected = select_workflows(&workflows, None, Some(&sort));

        assert_eq!(selected[0].name, "High");
    }
}
