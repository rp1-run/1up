use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryIntent {
    Definition,
    Flow,
    Usage,
    Docs,
    General,
}

const DEFINITION_SIGNALS: &[&str] = &[
    "define",
    "definition",
    "struct",
    "class",
    "trait",
    "interface",
    "type",
    "enum",
    "fn ",
    "func ",
    "function ",
    "declare",
    "schema",
    "model",
    "what is",
    "where is",
];

const FLOW_SIGNALS: &[&str] = &[
    "flow",
    "pipeline",
    "workflow",
    "sequence",
    "orchestrat",
    "lifecycle",
    "process",
    "how does",
    "how is",
    "control flow",
    "call chain",
    "data flow",
    "request flow",
];

const USAGE_SIGNALS: &[&str] = &[
    "usage",
    "use ",
    "used",
    "using",
    "call",
    "invoke",
    "import",
    "reference",
    "depend",
    "consumer",
    "caller",
    "who calls",
    "where is .* used",
    "example",
];

const DOCS_SIGNALS: &[&str] = &[
    "doc",
    "readme",
    "comment",
    "explain",
    "description",
    "overview",
    "guide",
    "tutorial",
    "help",
    "api doc",
    "documentation",
];

pub fn detect_intent(query: &str) -> QueryIntent {
    let lower = query.to_lowercase();

    let def_score = score_signals(&lower, DEFINITION_SIGNALS);
    let flow_score = score_signals(&lower, FLOW_SIGNALS);
    let usage_score = score_signals(&lower, USAGE_SIGNALS);
    let docs_score = score_signals(&lower, DOCS_SIGNALS);

    let max_score = def_score.max(flow_score).max(usage_score).max(docs_score);

    if max_score == 0 {
        return QueryIntent::General;
    }

    if def_score == max_score {
        QueryIntent::Definition
    } else if flow_score == max_score {
        QueryIntent::Flow
    } else if usage_score == max_score {
        QueryIntent::Usage
    } else {
        QueryIntent::Docs
    }
}

fn score_signals(query: &str, signals: &[&str]) -> usize {
    signals.iter().filter(|s| query.contains(**s)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_definition_intent() {
        assert_eq!(
            detect_intent("where is the struct Config defined"),
            QueryIntent::Definition
        );
        assert_eq!(
            detect_intent("find function definition for parse"),
            QueryIntent::Definition
        );
    }

    #[test]
    fn detects_flow_intent() {
        assert_eq!(
            detect_intent("how does the request flow work"),
            QueryIntent::Flow
        );
        assert_eq!(
            detect_intent("pipeline orchestration sequence"),
            QueryIntent::Flow
        );
    }

    #[test]
    fn detects_usage_intent() {
        assert_eq!(
            detect_intent("who calls this function and where is it used"),
            QueryIntent::Usage
        );
    }

    #[test]
    fn detects_docs_intent() {
        assert_eq!(
            detect_intent("show me the api documentation and guide"),
            QueryIntent::Docs
        );
    }

    #[test]
    fn defaults_to_general() {
        assert_eq!(
            detect_intent("error handling network"),
            QueryIntent::General
        );
        assert_eq!(detect_intent(""), QueryIntent::General);
    }
}
