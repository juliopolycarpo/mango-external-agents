//! The smoke host answers the vendor's questions from its terminal.
//!
//! Separate from [`terminal`](crate::terminal), which brokers *permissions*, and the separation is
//! the point rather than tidiness. Answering a question here grants the agent nothing — see
//! [`InteractionKind::grants_authority`](mango_external_agents::InteractionKind::grants_authority)
//! — so none of this goes near a [`PermissionBroker`](mango_external_agents::PermissionBroker),
//! and a "yes" typed at one of these prompts can never become an approval.

use std::io::{IsTerminal, Write};

use mango_external_agents::{
    Answer, AnswerValue, Question, QuestionForm, QuestionRequest, QuestionResponse,
};

/// What this host types back when nobody is at the keyboard.
///
/// A required question cannot be declined — the vendor is waiting on it — so it is answered with
/// the vendor's own first choice, or with a string that is obviously not a person's.
const SYNTHETIC_TEXT: &str = "mea: no answer available";

/// Answers one round of questions, asking whoever is at the terminal.
///
/// # Example
///
/// ```text
/// mea turn --harness codex "which branch should I target?"
/// ```
pub(crate) async fn answer_round(request: &QuestionRequest) -> QuestionResponse {
    let mut answers = Vec::with_capacity(request.questions.len());
    for question in &request.questions {
        answers.push(Answer::new(question.id.clone(), answer_one(question).await));
    }
    QuestionResponse::new(request.interaction.id.clone(), answers)
}

/// One answer, read from the terminal when there is one and derived when there is not.
async fn answer_one(question: &Question) -> AnswerValue {
    if !std::io::stdin().is_terminal() {
        return offline_answer(question);
    }
    let prompt = prompt_for(question);
    let asked = question.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // A detached input thread cannot keep the async runtime alive past the round's deadline, which
    // is the same reason the permission broker beside this one reads on a thread of its own.
    std::thread::spawn(move || {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut typed = String::new();
        let _ = std::io::stdin().read_line(&mut typed);
        let _ = sender.send(read_answer(&asked, &typed));
    });
    receiver.await.unwrap_or_else(|_| offline_answer(question))
}

/// The prompt one question renders as, with its choices numbered.
fn prompt_for(question: &Question) -> String {
    let mut prompt = String::new();
    if let Some(detail) = &question.detail {
        prompt.push_str(detail);
        prompt.push('\n');
    }
    prompt.push_str(&question.prompt);
    prompt.push('\n');
    if let QuestionForm::Choice { options, .. } = &question.form {
        for (index, option) in options.iter().enumerate() {
            let label = option
                .label
                .as_deref()
                .unwrap_or_else(|| option.id.as_str());
            prompt.push_str(&format!("  {}) {label}\n", index + 1));
        }
    }
    prompt.push_str(match (&question.form, question.required) {
        (QuestionForm::Choice { .. }, true) => "Choose a number: ",
        (QuestionForm::Choice { .. }, false) => "Choose a number, or nothing to decline: ",
        (QuestionForm::FreeText { .. }, true) => "Answer: ",
        (QuestionForm::FreeText { .. }, false) => "Answer, or nothing to decline: ",
        // `QuestionForm` is `#[non_exhaustive]`: a shape this build cannot render is one it must
        // not pretend to have read, so the prompt says only what it knows.
        (_, true) => "Answer: ",
        (_, false) => "Answer, or nothing to decline: ",
    });
    prompt
}

/// What somebody typed, as an answer to the question they were shown.
///
/// A number outside the offered range declines rather than picking a neighbour: the vendor would
/// refuse an option it never offered, and picking the nearest one would answer a different
/// question than the person meant.
fn read_answer(question: &Question, typed: &str) -> AnswerValue {
    let typed = typed.trim();
    if typed.is_empty() {
        return match question.required {
            true => offline_answer(question),
            false => AnswerValue::Declined,
        };
    }
    match &question.form {
        QuestionForm::Choice { options, .. } => typed
            .parse::<usize>()
            .ok()
            .and_then(|choice| choice.checked_sub(1))
            .and_then(|index| options.get(index))
            .map_or(AnswerValue::Declined, |option| {
                AnswerValue::chosen(option.id.clone())
            }),
        QuestionForm::FreeText { .. } => AnswerValue::text(typed),
        // A form arm this build does not know: whatever was typed is text, which is the one thing
        // that is true of every answer shape.
        _ => AnswerValue::text(typed),
    }
}

/// The answer this host gives with nobody at the keyboard.
fn offline_answer(question: &Question) -> AnswerValue {
    if !question.required {
        return AnswerValue::Declined;
    }
    match &question.form {
        QuestionForm::Choice { options, .. } => {
            options.first().map_or(AnswerValue::Declined, |option| {
                AnswerValue::chosen(option.id.clone())
            })
        }
        // Free text, and the `#[non_exhaustive]` tail: a shape this build cannot render still
        // needs an answer, and a synthetic string is one no person would have typed.
        _ => AnswerValue::text(SYNTHETIC_TEXT),
    }
}

#[cfg(test)]
mod tests {
    use mango_external_agents::{
        AnswerValue, Question, QuestionForm, QuestionId, QuestionOption, QuestionOptionId,
    };

    use super::{offline_answer, prompt_for, read_answer};

    fn choice(required: bool) -> Question {
        let question = Question::new(
            QuestionId::new("branch"),
            "Which branch?",
            QuestionForm::Choice {
                options: vec![
                    QuestionOption::new(QuestionOptionId::new("main")).with_label("main"),
                    QuestionOption::new(QuestionOptionId::new("next")).with_label("next"),
                ],
                multi_select: false,
            },
        );
        match required {
            true => question.required(),
            false => question,
        }
    }

    fn free_text(required: bool) -> Question {
        let question = Question::new(
            QuestionId::new("message"),
            "What should the commit say?",
            QuestionForm::FreeText { placeholder: None },
        );
        match required {
            true => question.required(),
            false => question,
        }
    }

    #[test]
    fn a_number_picks_the_option_it_names_by_the_vendors_own_id() {
        assert_eq!(
            read_answer(&choice(false), "2\n"),
            AnswerValue::chosen(QuestionOptionId::new("next"))
        );
    }

    /// The vendor would refuse an option it never offered, and the nearest one answers a different
    /// question than the person meant.
    #[test]
    fn a_number_outside_the_offered_range_declines_rather_than_picking_a_neighbour() {
        for typed in ["0", "3", "-1", "main"] {
            assert_eq!(
                read_answer(&choice(false), typed),
                AnswerValue::Declined,
                "received an answer for {typed:?}"
            );
        }
    }

    #[test]
    fn empty_input_declines_an_optional_question_and_answers_a_required_one() {
        assert_eq!(read_answer(&free_text(false), "\n"), AnswerValue::Declined);
        assert!(!read_answer(&free_text(true), "\n").is_declined());
    }

    /// Declining is not an option the vendor left open on a required question, so this host says
    /// something rather than hanging the turn — and says something no person would type.
    #[test]
    fn a_required_question_is_never_left_unanswered_with_nobody_at_the_keyboard() {
        assert_eq!(
            offline_answer(&choice(true)),
            AnswerValue::chosen(QuestionOptionId::new("main"))
        );
        assert!(!offline_answer(&free_text(true)).is_declined());
        assert!(offline_answer(&choice(false)).is_declined());
    }

    #[test]
    fn the_prompt_numbers_the_choices_it_offers_and_says_whether_declining_is_allowed() {
        let prompt = prompt_for(&choice(false));
        assert!(prompt.contains("1) main"), "received {prompt:?}");
        assert!(prompt.contains("2) next"), "received {prompt:?}");
        assert!(prompt.contains("decline"), "received {prompt:?}");
        assert!(
            !prompt_for(&choice(true)).contains("decline"),
            "a required question offers no decline"
        );
    }
}
