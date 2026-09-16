//! Typed wizard questions: the ≤6 core questions a preset asks, derived from
//! the engine rather than authored in the UI.
//!
//! A [`Question`] names the [`crate::WizardAnswers`] field it feeds (`id`),
//! so a client can render a form purely from [`questions_for`]'s output and
//! post back a [`crate::WizardAnswers`]-shaped payload keyed by those same
//! ids, with no question text or option list hardcoded in TypeScript.

use serde::{Deserialize, Serialize};

use crate::preset::{Preset, PresetConstraint};

/// The kind of answer a [`Question`] expects, carrying whatever bounds or
/// options the UI needs to render the right input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKind {
    /// Free-form short text.
    Text,
    /// A bounded numeric input.
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        /// Display unit, e.g. `"tokens"`, `"requests"`, `"ms"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// A single choice from a fixed option list.
    Choice { options: Vec<String> },
    /// A yes/no toggle.
    Boolean,
    /// A monetary amount plus currency.
    Money,
}

/// One wizard question. `id` names the [`crate::WizardAnswers`] field this
/// question's answer feeds (e.g. `"context_length"`, `"priority"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// The [`crate::WizardAnswers`] field this answer feeds.
    pub id: String,
    /// The question text shown to the user.
    pub prompt: String,
    pub kind: QuestionKind,
    /// Whether an answer must be supplied (vs. an optional refinement).
    pub required: bool,
    /// Whether answering this question (when truthy / non-default) turns into
    /// a *hard*, filtering constraint downstream in [`crate::rank::rank`],
    /// rather than only a soft ranking preference. `false` for every question
    /// that only ever influences ranking or display.
    pub hard: bool,
    /// Optional helper text expanding on `prompt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Question {
    fn new(id: &str, prompt: &str, kind: QuestionKind, required: bool, hard: bool) -> Self {
        Self {
            id: id.to_string(),
            prompt: prompt.to_string(),
            kind,
            required,
            hard,
            help: None,
        }
    }

    #[must_use]
    fn with_help(mut self, help: &str) -> Self {
        self.help = Some(help.to_string());
        self
    }
}

/// The ≤6 core wizard questions for `preset`, mapped 1:1 onto
/// [`crate::WizardAnswers`] fields.
///
/// `context_length` and `concurrency` are always asked (every preset's
/// candidate template is parametrized by both). `eu_residency_required` is
/// asked only for a preset that does not already carry
/// [`PresetConstraint::EuResidencyOnly`] as a fixed hard constraint - asking
/// it there would offer a choice the preset has already made. `max_latency_ms`,
/// `budget`, and `priority` are always asked, as optional refinements.
#[must_use]
pub fn questions_for(preset: &Preset) -> Vec<Question> {
    let mut questions = vec![
        Question::new(
            "context_length",
            "How much context (tokens) does the model need to hold?",
            QuestionKind::Number {
                min: Some(256.0),
                max: Some(131_072.0),
                unit: Some("tokens".to_string()),
            },
            true,
            false,
        )
        .with_help("Prompt plus expected generation length. Longer contexts need more memory."),
        Question::new(
            "concurrency",
            "How many concurrent requests should this stack handle?",
            QuestionKind::Number {
                min: Some(1.0),
                max: Some(100_000.0),
                unit: Some("requests".to_string()),
            },
            true,
            false,
        )
        .with_help("Drives how many accelerators / how much sharding the candidate needs."),
    ];

    if !preset
        .constraints
        .contains(&PresetConstraint::EuResidencyOnly)
    {
        questions.push(
            Question::new(
                "eu_residency_required",
                "Must data and compute stay within the EU?",
                QuestionKind::Boolean,
                false,
                true,
            )
            .with_help(
                "When set, only EU-resident providers are ever proposed - this becomes a hard \
                 constraint, not just a preference.",
            ),
        );
    }

    questions.push(
        Question::new(
            "max_latency_ms",
            "Is there a latency ceiling this stack must meet?",
            QuestionKind::Number {
                min: Some(1.0),
                max: None,
                unit: Some("ms".to_string()),
            },
            false,
            false,
        )
        .with_help("Optional. Leave blank if there is no hard latency requirement."),
    );

    questions.push(
        Question::new(
            "budget",
            "Is there a monthly spend ceiling?",
            QuestionKind::Money,
            false,
            false,
        )
        .with_help("Optional. When set, the generated config carries a monthly budget alert."),
    );

    questions.push(
        Question::new(
            "priority",
            "What matters most when we rank candidates for you?",
            QuestionKind::Choice {
                options: vec![
                    "cost".to_string(),
                    "latency".to_string(),
                    "quality".to_string(),
                    "privacy".to_string(),
                    "simplicity".to_string(),
                ],
            },
            false,
            false,
        )
        .with_help("Optional. Leave unset for balanced weighting across every axis."),
    );

    questions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn every_preset_asks_six_or_fewer_questions() {
        for preset in catalog() {
            let qs = questions_for(preset);
            assert!(
                !qs.is_empty() && qs.len() <= 6,
                "{} asked {} questions",
                preset.id,
                qs.len()
            );
        }
    }

    #[test]
    fn every_question_id_maps_onto_a_wizard_answers_field() {
        const KNOWN_FIELDS: &[&str] = &[
            "context_length",
            "concurrency",
            "eu_residency_required",
            "max_latency_ms",
            "budget",
            "priority",
        ];
        for preset in catalog() {
            for q in questions_for(preset) {
                assert!(
                    KNOWN_FIELDS.contains(&q.id.as_str()),
                    "{} asked unknown field {:?}",
                    preset.id,
                    q.id
                );
            }
        }
    }

    #[test]
    fn eu_residency_only_presets_do_not_ask_the_eu_question_and_stay_at_five() {
        let eu = catalog()
            .iter()
            .find(|p| p.id.as_str() == "private_eu_production")
            .expect("eu preset");
        let qs = questions_for(eu);
        assert!(!qs.iter().any(|q| q.id == "eu_residency_required"));
        assert_eq!(qs.len(), 5);
    }

    #[test]
    fn non_eu_hard_presets_ask_the_eu_question_as_hard() {
        for id in ["local_dev_chat", "cost_optimized_batch"] {
            let preset = catalog()
                .iter()
                .find(|p| p.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} in catalog"));
            let qs = questions_for(preset);
            let eu_q = qs
                .iter()
                .find(|q| q.id == "eu_residency_required")
                .unwrap_or_else(|| panic!("{id} should ask eu_residency_required"));
            assert!(
                eu_q.hard,
                "{id}'s eu_residency_required question should be hard"
            );
            assert_eq!(qs.len(), 6);
        }
    }

    #[test]
    fn required_questions_are_always_context_and_concurrency() {
        for preset in catalog() {
            let required_ids: Vec<String> = questions_for(preset)
                .into_iter()
                .filter(|q| q.required)
                .map(|q| q.id)
                .collect();
            assert_eq!(required_ids, vec!["context_length", "concurrency"]);
        }
    }
}
