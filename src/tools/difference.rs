use crate::{
    errors::{IntoDiagnostic, Result},
    tools::ui,
    utils::PathExt,
};
use std::{collections::HashMap, path::Path, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Difference {
    Presence { expected: bool, actual: bool },
    StringContent { expected: String, actual: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Comparison {
    pub label: String,
    pub expected: PathBuf,
    pub actual: PathBuf,
}

impl Comparison {
    pub fn from_xxx_json(path: &Path) -> Result<Self> {
        let base_name = path.extract_filename()?.replace(".json.new", "").replace(".json", "");
        let label = path.with_file_name(&base_name).to_string_lossy().to_string();
        Ok(Self {
            expected: path.with_file_name(format!("{base_name}.json")),
            actual: path.with_file_name(format!("{base_name}.json.new")),
            label,
        })
    }
}

pub fn find_differences(files: &Vec<PathBuf>) -> Result<HashMap<Comparison, Difference>> {
    let mut differences = HashMap::new();
    for path in files {
        let filename = path.extract_filename()?;
        if filename.ends_with(".json.new") {
            let comparison = Comparison::from_xxx_json(path)?;
            if comparison.expected.exists() {
                let expected_content =
                    std::fs::read_to_string(&comparison.expected).into_diagnostic()?;
                let actual_content =
                    std::fs::read_to_string(&comparison.actual).into_diagnostic()?;
                if expected_content != actual_content {
                    differences.insert(
                        comparison,
                        Difference::StringContent {
                            expected: expected_content,
                            actual: actual_content,
                        },
                    );
                }
            } else {
                differences
                    .insert(comparison, Difference::Presence { expected: false, actual: true });
            }
        } else if filename.ends_with(".json") {
            let comparison = Comparison::from_xxx_json(path)?;
            if !comparison.actual.exists() {
                differences
                    .insert(comparison, Difference::Presence { expected: true, actual: false });
            }
        }
    }
    Ok(differences)
}

impl Difference {
    pub fn show(&self, comparison: &Comparison) -> Result<()> {
        let label = &comparison.label;
        match self {
            Difference::Presence { expected, actual } => {
                if *expected && !*actual {
                    cliclack::log::warning(format!("missing: {label}")).into_diagnostic()?;
                } else {
                    cliclack::log::warning(format!("unexpected : {label}")).into_diagnostic()?;
                }
            }
            Difference::StringContent { expected, actual } => {
                cliclack::log::warning(format!("difference detected on: {label}\n"))
                    .into_diagnostic()?;
                ui::show_difference_text(expected, actual, true)?;
            }
        }
        Ok(())
    }

    /// return true when the new/actual state is accepted (and replace the old one)
    pub fn review(&self, comparison: &Comparison) -> Result<bool> {
        let accept_update = match self {
            Difference::Presence { expected, actual } => {
                if *expected && !actual {
                    if crate::tools::ui::ask_to_update_sample(&format!(
                        "Accept to remove existing {}?",
                        &comparison.label
                    ))? {
                        std::fs::remove_file(&comparison.expected).into_diagnostic()?;
                        true
                    } else {
                        false
                    }
                } else if crate::tools::ui::ask_to_update_sample(&format!(
                    "Accept to add new {}?",
                    &comparison.label
                ))? {
                    std::fs::rename(&comparison.actual, &comparison.expected).into_diagnostic()?;
                    true
                } else {
                    std::fs::remove_file(&comparison.actual).into_diagnostic()?;
                    false
                }
            }
            Difference::StringContent { expected, actual } => {
                crate::tools::ui::show_difference_text(expected, actual, true)?;
                if crate::tools::ui::ask_to_update_sample(&format!(
                    "Accept to update {}?",
                    comparison.label
                ))? {
                    std::fs::rename(&comparison.actual, &comparison.expected).into_diagnostic()?;
                    true
                } else {
                    std::fs::remove_file(&comparison.actual).into_diagnostic()?;
                    false
                }
            }
        };
        Ok(accept_update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_build_comparison() {
        let comparison = Comparison::from_xxx_json(Path::new("toto/bar/foo.json.new")).unwrap();
        assert_eq!(comparison.label, "toto/bar/foo");
        assert_eq!(comparison.actual, Path::new("toto/bar/foo.json.new"));
        assert_eq!(comparison.expected, Path::new("toto/bar/foo.json"));
    }

    #[test]
    fn find_no_differences() {
        let tmpdir = tempfile::tempdir().unwrap();
        let actual = tmpdir.path().join("foo.json.new");
        std::fs::write(&actual, "{}").unwrap();
        let expected = tmpdir.path().join("foo.json");
        std::fs::write(&expected, "{}").unwrap();
        let files = vec![actual.clone(), expected.clone()];

        let diffs = find_differences(&files).unwrap();
        assert_eq!(diffs.len(), 0);
    }

    #[test]
    fn find_differences_on_content() {
        let tmpdir = tempfile::tempdir().unwrap();
        let actual = tmpdir.path().join("foo.json.new");
        std::fs::write(&actual, "{}").unwrap();
        let expected = tmpdir.path().join("foo.json");
        std::fs::write(&expected, "[]").unwrap();
        let files = vec![actual.clone(), expected.clone()];

        let diffs = find_differences(&files).unwrap();
        assert_eq!(diffs.len(), 1);
        let (comparison, diff) = diffs.into_iter().next().unwrap();
        assert!(comparison.label.ends_with("foo"));
        assert_eq!(comparison.actual, actual);
        assert_eq!(comparison.expected, expected);
        assert_eq!(
            diff,
            Difference::StringContent { actual: "{}".to_string(), expected: "[]".to_string() }
        );
    }

    #[test]
    fn find_non_expected() {
        let tmpdir = tempfile::tempdir().unwrap();
        let actual = tmpdir.path().join("foo.json.new");
        std::fs::write(&actual, "{}").unwrap();
        // let expected = tmpdir.path().join("foo.json");
        // std::fs::write(&expected, "{}").unwrap();
        let files = vec![actual.clone()];

        let diffs = find_differences(&files).unwrap();
        assert_eq!(diffs.len(), 1);
        let (comparison, diff) = diffs.into_iter().next().unwrap();
        assert!(comparison.label.ends_with("foo"));
        assert_eq!(comparison.actual, actual);
        assert_eq!(diff, Difference::Presence { actual: true, expected: false });
    }

    #[test]
    fn find_missing() {
        let tmpdir = tempfile::tempdir().unwrap();
        // let actual = tmpdir.path().join("foo.json.new");
        // std::fs::write(&actual, "{}").unwrap();
        let expected = tmpdir.path().join("foo.json");
        std::fs::write(&expected, "{}").unwrap();
        let files = vec![expected.clone()];

        let diffs = find_differences(&files).unwrap();
        assert_eq!(diffs.len(), 1);
        let (comparison, diff) = diffs.into_iter().next().unwrap();
        assert!(comparison.label.ends_with("foo"));
        assert_eq!(comparison.expected, expected);
        assert_eq!(diff, Difference::Presence { actual: false, expected: true });
    }
}
