//! The smoke host answers the vendor's questions from its terminal.
//!
//! Separate from [`terminal`](crate::terminal), which brokers *permissions*, and the separation is
//! the point rather than tidiness. Answering a question here grants the agent nothing — see
//! [`InteractionKind::grants_authority`](mango_external_agents::InteractionKind::grants_authority)
//! — so none of this goes near a [`PermissionBroker`](mango_external_agents::PermissionBroker),
//! and a "yes" typed at one of these prompts can never become an approval.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

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
///
/// One worker owns standard input for this turn. A question that ends while `read_line` is blocked
/// drops its receiver, so the worker discards that line before it renders the next question. A
/// second reader would race the old one and could consume a line meant for the next prompt.
#[derive(Clone)]
pub(crate) struct TerminalInput {
    requests: mpsc::Sender<InputRequest>,
}

struct InputRequest {
    prompt: String,
    answer: tokio::sync::oneshot::Sender<String>,
    cancelled: Arc<AtomicBool>,
}

struct PendingInput {
    cancelled: Arc<AtomicBool>,
    completed: bool,
}

impl Drop for PendingInput {
    fn drop(&mut self) {
        if self.completed || self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        eprintln!(
            "\nInput cancelled. Press Enter to discard the current line; the next prompt will then appear."
        );
    }
}

impl TerminalInput {
    pub(crate) fn new() -> Self {
        let (requests, receiver) = mpsc::channel();
        std::thread::spawn(move || read_terminal_lines(receiver));
        Self { requests }
    }

    /// Renders `prompt` and waits for one line, if this process owns a terminal.
    pub(crate) async fn prompt_line(&self, prompt: &str) -> Option<String> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let (answer, receiver) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.requests
            .send(InputRequest {
                prompt: prompt.to_owned(),
                answer,
                cancelled: Arc::clone(&cancelled),
            })
            .ok()?;
        let mut pending = PendingInput {
            cancelled,
            completed: false,
        };
        let typed = receiver.await.ok();
        pending.completed = typed.is_some();
        typed
    }
}

/// The sole owner of the blocking standard-input read for one terminal input object.
fn read_terminal_lines(requests: mpsc::Receiver<InputRequest>) {
    deliver_terminal_lines(requests, || {
        let mut typed = String::new();
        let _ = std::io::stdin().read_line(&mut typed);
        typed
    });
}

/// Renders requests in order and drops a line whose request ended while it was being read.
fn deliver_terminal_lines(
    requests: mpsc::Receiver<InputRequest>,
    mut read_line: impl FnMut() -> String,
) {
    while let Ok(request) = requests.recv() {
        if request.cancelled.load(Ordering::Acquire) {
            continue;
        }
        eprint!("{}", request.prompt);
        let _ = std::io::stderr().flush();
        let typed = read_line();
        // The receiver is gone when the event loop withdrew the prompt. Dropping its line keeps a
        // cancelled question from becoming the answer to whichever prompt arrives next.
        if !request.cancelled.load(Ordering::Acquire) {
            let _ = request.answer.send(typed);
        }
    }
}

#[async_trait::async_trait]
impl QuestionInput for TerminalInput {
    async fn read_line(&self, prompt: &str) -> Option<String> {
        self.prompt_line(prompt).await
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
    request: QuestionRequest,
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, SystemTime};

    use mango_external_agents::interaction::ANSWER_TEXT_MAX_LENGTH;
    use mango_external_agents::{
        AnswerValue, Interaction, InteractionId, InteractionKind, Question, QuestionForm,
        QuestionId, QuestionOption, QuestionOptionId, QuestionRequest, SessionId,
    };

    use super::{
        InputRequest, QuestionInput, answer_round, deliver_terminal_lines, offline_answer,
        prompt_for, read_answer,
    };

    /// A keyboard that types the same line at every prompt, or nobody at all.
    struct ScriptedInput(Option<String>);

    #[async_trait::async_trait]
    impl QuestionInput for ScriptedInput {
        async fn read_line(&self, _prompt: &str) -> Option<String> {
            self.0.clone()
        }
    }

    /// A named stand-in for the one blocking line source the terminal worker owns.
    struct FakeTerminalLines {
        lines: VecDeque<String>,
        cancel_during_first_read: Arc<AtomicBool>,
        reads: usize,
    }

    impl FakeTerminalLines {
        fn read_line(&mut self) -> String {
            self.reads += 1;
            if self.reads == 1 {
                self.cancel_during_first_read.store(true, Ordering::Release);
            }
            self.lines
                .pop_front()
                .expect("expected a line for each pending terminal read")
        }
    }

    #[tokio::test]
    async fn a_cancelled_read_discards_its_line_before_the_next_prompt_reads() {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (first_answer, first_response) = tokio::sync::oneshot::channel();
        let (second_answer, second_response) = tokio::sync::oneshot::channel();
        sender
            .send(InputRequest {
                prompt: String::from("first: "),
                answer: first_answer,
                cancelled: Arc::clone(&cancelled),
            })
            .expect("expected the first prompt to reach the worker");
        sender
            .send(InputRequest {
                prompt: String::from("second: "),
                answer: second_answer,
                cancelled: Arc::new(AtomicBool::new(false)),
            })
            .expect("expected the second prompt to reach the worker");
        drop(sender);

        let mut lines = FakeTerminalLines {
            lines: VecDeque::from([String::from("old line\n"), String::from("new line\n")]),
            cancel_during_first_read: cancelled,
            reads: 0,
        };
        deliver_terminal_lines(receiver, || lines.read_line());

        assert!(
            first_response.await.is_err(),
            "the line read after cancellation must not answer the old prompt"
        );
        assert_eq!(
            second_response.await.expect("expected the second answer"),
            "new line\n",
            "the next prompt must read its own line after the cancelled one was discarded"
        );
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
        let response = answer_round(&ScriptedInput(Some(typed)), request.clone()).await;

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
            let response =
                answer_round(&ScriptedInput(typed.map(str::to_owned)), request.clone()).await;
            request
                .validate(&response)
                .unwrap_or_else(|error| panic!("typing {typed:?} produced {error}"));
        }
    }
}
