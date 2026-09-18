//! The smoke host answers the vendor's questions from its terminal.
//!
//! Separate from [`terminal`](crate::terminal), which brokers *permissions*, and the separation is
//! the point rather than tidiness. Answering a question here grants the agent nothing — see
//! [`InteractionKind::grants_authority`](mango_external_agents::InteractionKind::grants_authority)
//! — so none of this goes near a [`PermissionBroker`](mango_external_agents::PermissionBroker),
//! and a "yes" typed at one of these prompts can never become an approval.

use std::io::{IsTerminal, Write};

use mango_external_agents::interaction::ANSWER_TEXT_MAX_LENGTH;
use mango_external_agents::{
    Answer, AnswerValue, Question, QuestionForm, QuestionRequest, QuestionResponse,
};

/// What this host types back when nobody is at the keyboard.
///
/// A required question cannot be declined — the vendor is waiting on it — so it is answered with
/// the vendor's own first choice, or with a string that is obviously not a person's.
const SYNTHETIC_TEXT: &str = "mea: no answer available";

/// Where a typed answer comes from.
///
/// A trait rather than a direct `stdin` read, because `is_terminal()` is process state a test
/// cannot set: a suite run from an interactive shell would block an OS thread in `read_line`, and
/// the same suite passing under a pipe would be passing for a reason that has nothing to do with
/// what it checks.
#[async_trait::async_trait]
pub(crate) trait QuestionInput: Send + Sync {
    /// One line in answer to `prompt`, or `None` when nobody is there to type one.
    async fn read_line(&self, prompt: &str) -> Option<String>;
}

/// The person at the terminal, when there is one.
pub(crate) struct TerminalInput;

#[async_trait::async_trait]
impl QuestionInput for TerminalInput {
    async fn read_line(&self, prompt: &str) -> Option<String> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let prompt = prompt.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // A detached input thread cannot keep the async runtime alive past the round's deadline,
        // which is the same reason the permission broker beside this one reads on a thread of its
        // own.
        std::thread::spawn(move || {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
            let mut typed = String::new();
            let _ = std::io::stdin().read_line(&mut typed);
            let _ = sender.send(typed);
        });
        receiver.await.ok()
    }
}

/// Answers one round of questions, asking whoever is at the terminal.
///
/// The answers are checked against the questions before they leave. A refused answer leaves the
/// round open and the turn waiting — `Session::answer` validates on receive, and a vendor that
/// refused one has still not been answered — so a set that does not pass is replaced by the one
/// this host would have given with nobody at the keyboard, which always does.
///
/// # Example
///
/// ```text
/// mea turn --harness codex "which branch should I target?"
/// ```
pub(crate) async fn answer_round(
    input: &dyn QuestionInput,
    request: &QuestionRequest,
) -> QuestionResponse {
    let mut answers = Vec::with_capacity(request.questions.len());
    for question in &request.questions {
        answers.push(Answer::new(
            question.id.clone(),
            answer_one(input, question).await,
        ));
    }
    let response = QuestionResponse::new(request.interaction.id.clone(), answers);
    if request.validate(&response).is_ok() {
        return response;
    }
    QuestionResponse::new(
        request.interaction.id.clone(),
        request
            .questions
            .iter()
            .map(|question| Answer::new(question.id.clone(), offline_answer(question)))
            .collect(),
    )
}

/// One answer, read from the terminal when there is one and derived when there is not.
async fn answer_one(input: &dyn QuestionInput, question: &Question) -> AnswerValue {
    match input.read_line(&prompt_for(question)).await {
        Some(typed) => read_answer(question, &typed),
        None => offline_answer(question),
    }
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
/// Anything unusable falls back to what this host would have said with nobody at the keyboard:
/// a decline where the vendor allows one, an answer where it does not. A number outside the
/// offered range is unusable rather than rounded — the vendor would refuse an option it never
/// offered, and the nearest one answers a different question than the person meant.
fn read_answer(question: &Question, typed: &str) -> AnswerValue {
    let typed = typed.trim();
    if typed.is_empty() {
        return offline_answer(question);
    }
    match &question.form {
        QuestionForm::Choice { options, .. } => typed
            .parse::<usize>()
            .ok()
            .and_then(|choice| choice.checked_sub(1))
            .and_then(|index| options.get(index))
            .map_or_else(
                || offline_answer(question),
                |option| AnswerValue::chosen(option.id.clone()),
            ),
        // Bounded here rather than left to the vendor: an answer past the ceiling is refused on
        // receive, which leaves the round open and the turn waiting on a question nobody gets to
        // answer a second time.
        _ => AnswerValue::text(bounded(typed)),
    }
}

/// The answer this host gives with nobody at the keyboard.
fn offline_answer(question: &Question) -> AnswerValue {
    if !question.required {
        return AnswerValue::Declined;
    }
    match &question.form {
        QuestionForm::Choice { options, .. } => options.first().map_or_else(
            || AnswerValue::text(SYNTHETIC_TEXT),
            |option| AnswerValue::chosen(option.id.clone()),
        ),
        // Free text, and the `#[non_exhaustive]` tail: a shape this build cannot render still
        // needs an answer, and a synthetic string is one no person would have typed.
        _ => AnswerValue::text(SYNTHETIC_TEXT),
    }
}

/// The first [`ANSWER_TEXT_MAX_LENGTH`] characters, never cutting one in half.
fn bounded(text: &str) -> &str {
    text.char_indices()
        .nth(ANSWER_TEXT_MAX_LENGTH)
        .map_or(text, |(boundary, _)| &text[..boundary])
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use mango_external_agents::interaction::ANSWER_TEXT_MAX_LENGTH;
    use mango_external_agents::{
        AnswerValue, Interaction, InteractionId, InteractionKind, Question, QuestionForm,
        QuestionId, QuestionOption, QuestionOptionId, QuestionRequest, SessionId,
    };

    use super::{QuestionInput, answer_round, offline_answer, prompt_for, read_answer};

    /// A keyboard that types the same line at every prompt, or nobody at all.
    struct ScriptedInput(Option<String>);

    #[async_trait::async_trait]
    impl QuestionInput for ScriptedInput {
        async fn read_line(&self, _prompt: &str) -> Option<String> {
            self.0.clone()
        }
    }

    fn choice(required: bool) -> Question {
        named_choice("branch", required)
    }

    fn named_choice(id: &str, required: bool) -> Question {
        let question = Question::new(
            QuestionId::new(id),
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

    fn round(questions: Vec<Question>) -> QuestionRequest {
        QuestionRequest::new(
            Interaction::new(
                InteractionId::new("ask-1"),
                InteractionKind::Question,
                SessionId::new("session-1"),
                SystemTime::now() + Duration::from_secs(60),
            ),
            questions,
        )
    }

    #[test]
    fn a_number_picks_the_option_it_names_by_the_vendors_own_id() {
        assert_eq!(
            read_answer(&choice(false), "2\n"),
            AnswerValue::chosen(QuestionOptionId::new("next"))
        );
    }

    /// The vendor would refuse an option it never offered, and the nearest one answers a different
    /// question than the person meant. For an optional question that means declining; for a
    /// required one it means the vendor's own first choice, because declining is not on offer and
    /// an answer the vendor refuses leaves the turn waiting on a question nobody answers twice.
    #[test]
    fn unusable_input_declines_where_it_may_and_answers_where_it_must() {
        for typed in ["0", "3", "-1", "main"] {
            assert_eq!(
                read_answer(&choice(false), typed),
                AnswerValue::Declined,
                "received an answer for {typed:?}"
            );
            assert_eq!(
                read_answer(&choice(true), typed),
                AnswerValue::chosen(QuestionOptionId::new("main")),
                "a required choice cannot be declined, received {typed:?}"
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

    /// An answer past the ceiling is refused on receive, which leaves the round open and the turn
    /// waiting on a question nobody gets to answer again.
    #[tokio::test]
    async fn a_pasted_answer_longer_than_the_ceiling_is_cut_rather_than_refused() {
        let request = round(vec![free_text(true)]);
        let typed = "x".repeat(ANSWER_TEXT_MAX_LENGTH + 500);
        let response = answer_round(&ScriptedInput(Some(typed)), &request).await;

        request
            .validate(&response)
            .expect("expected an answer the vendor would take");
        let AnswerValue::Text { text } = &response.answers[0].value else {
            panic!("expected free text, received {response:?}");
        };
        assert_eq!(text.chars().count(), ANSWER_TEXT_MAX_LENGTH);
    }

    /// Every round this host sends has been checked against the round it answers, whatever was
    /// typed — so the one outcome that cannot happen is the turn waiting forever.
    #[tokio::test]
    async fn every_round_it_sends_is_one_the_request_would_take() {
        let request = round(vec![
            named_choice("branch", true),
            free_text(false),
            named_choice("run-tests", false),
        ]);
        for typed in [None, Some("1"), Some(""), Some("nonsense")] {
            let response = answer_round(&ScriptedInput(typed.map(str::to_owned)), &request).await;
            request
                .validate(&response)
                .unwrap_or_else(|error| panic!("typing {typed:?} produced {error}"));
        }
    }
}
