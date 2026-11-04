#[path = "support/evaluation.rs"]
mod evaluation;
#[test]
fn student_tasks_have_explicit_outcomes_and_sources() {
    let cases = evaluation::cases();
    assert!(cases.len() >= 6);
    for case in cases {
        assert!(!case.facts.is_empty());
        assert!(!case.citation_suffixes.is_empty());
        assert!(!case.required_tools.is_empty());
    }
}
