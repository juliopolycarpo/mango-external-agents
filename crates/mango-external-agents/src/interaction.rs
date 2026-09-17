//! Everything a vendor can stop and ask, and the lifecycle all of it shares.
//!
//! Two kinds of asking, deliberately not one type:
//!
//! - A **permission request** ([`PermissionRequest`](crate::PermissionRequest)) is an
//!   authorisation decision. Answering it lets the agent act.
//! - A **question** ([`QuestionRequest`]) is a request for information. Answering it tells the
//!   agent something, and grants it nothing.
//!
//! Collapsing the two is how a "which branch should I use?" prompt ends up rendered as, recorded
//! as, and policy-matched against an executable grant. [`InteractionKind`] keeps them apart on
//! every surface that carries both, and no [`PermissionBroker`](crate::PermissionBroker) is ever
//! consulted about a question.
//!
//! What they do share is [`Interaction`]: the id to answer with, whose session and turn it belongs
//! to, when it stops being answerable, and how it ended.
//!
//! Secret collection and arbitrary form rendering are outside what this library will carry. A
//! vendor asking for one is refused by name — [`UnsupportedQuestion`] — and never quietly reshaped
//! into free text, because a password typed into a box labelled "answer" is a password in a host's
//! transcript.

use std::fmt;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::event::SessionId;
use crate::normalize::{self, TextLimit};
use crate::operation::OperationRef;

/// How many questions one request may carry.
pub const QUESTION_MAX_ITEMS: usize = 16;

/// How many choices one question may offer.
pub const QUESTION_MAX_OPTIONS: usize = 32;

/// How many code points one free-text answer may carry.
pub const ANSWER_TEXT_MAX_LENGTH: usize = 4_096;

/// The vendor's own id for one thing it is waiting on.
///
/// Echoed back verbatim with the answer. Shared by permissions and questions so a host can hold
/// one map of what it is waiting on rather than two.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct InteractionId(String);

impl InteractionId {
    /// Names one interaction, exactly as the vendor spells it.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This id bounded, or a refusal when it could not be carried whole.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when the id does not survive bounding. Refused rather than
    /// cut: a shortened id answers a different question.
    pub fn normalized(self) -> Result<Self> {
        normalize::opaque_id(&self.0, "interaction id").map(Self)
    }
}

impl fmt::Display for InteractionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which of the two kinds of asking this is.
///
/// A plain question is not executable tool permission, and this is the field that says so on a
/// surface carrying both.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InteractionKind {
    /// An authorisation decision. Answering it lets the agent act.
    Permission,
    /// A request for information. Answering it grants nothing.
    Question,
}

impl InteractionKind {
    /// Whether answering this can authorise the agent to do something.
    ///
    /// The one question a host's audit trail and policy layer have to get right.
    pub const fn grants_authority(self) -> bool {
        matches!(self, Self::Permission)
    }
}

impl fmt::Display for InteractionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Permission => "permission",
            Self::Question => "question",
        })
    }
}

/// Where one interaction stands.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InteractionStatus {
    /// Nobody has answered and the deadline has not passed.
    Pending,
    /// Somebody answered.
    Resolved,
    /// Nobody answered in time.
    Expired,
    /// The turn or session ended before anybody answered.
    Cancelled,
    /// The library took the question back because it could not be carried.
    ///
    /// Distinct from [`InteractionStatus::Cancelled`]: nothing went wrong with the turn, the
    /// request itself was one this library will not put to a host.
    Withdrawn,
}

impl InteractionStatus {
    /// Whether an answer would still be accepted.
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// The lifecycle fields every interaction carries.
///
/// One shape for both kinds, so a host renders, times out and audits them through one path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Interaction {
    /// The vendor's own id, echoed back with the answer.
    pub id: InteractionId,
    /// Whether answering this grants authority.
    pub kind: InteractionKind,
    /// The session it belongs to.
    pub session_id: SessionId,
    /// The turn and attempt it belongs to, when a turn was running.
    ///
    /// Absent for a session-scoped ask. An answer that arrives naming an attempt the host has
    /// already replaced is answering work that no longer exists, which is what makes this worth
    /// carrying rather than deriving from whichever turn happens to be open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationRef>,
    /// When it stops being answerable.
    pub expires_at: SystemTime,
    /// Where it stands.
    pub status: InteractionStatus,
}

impl Interaction {
    /// A pending interaction in one session, with no turn of its own.
    pub fn new(
        id: InteractionId,
        kind: InteractionKind,
        session_id: SessionId,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            id,
            kind,
            session_id,
            operation: None,
            expires_at,
            status: InteractionStatus::Pending,
        }
    }

    /// Records which turn and attempt this belongs to.
    #[must_use]
    pub fn during(mut self, operation: OperationRef) -> Self {
        self.operation = Some(operation);
        self
    }

    /// Records where it ended up.
    #[must_use]
    pub fn resolved_as(mut self, status: InteractionStatus) -> Self {
        self.status = status;
        self
    }

    /// Whether an answer would still be accepted.
    pub const fn is_open(&self) -> bool {
        self.status.is_open()
    }

    /// This interaction with its vendor-written id bounded.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when the id does not survive bounding.
    pub fn normalized(self) -> Result<Self> {
        Ok(Self {
            id: self.id.normalized()?,
            ..self
        })
    }
}

/// The vendor's own id for one question inside a request.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct QuestionId(String);

impl QuestionId {
    /// Names one question, exactly as the vendor spells it.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuestionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The vendor's own id for one choice.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct QuestionOptionId(String);

impl QuestionOptionId {
    /// Names one choice, exactly as the vendor spells it.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuestionOptionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One choice a question offers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuestionOption {
    /// The vendor's own id, echoed back verbatim when this option is chosen.
    pub id: QuestionOptionId,
    /// The vendor's own label, rendered as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What the vendor says it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl QuestionOption {
    /// A choice with no label.
    pub fn new(id: QuestionOptionId) -> Self {
        Self {
            id,
            label: None,
            description: None,
        }
    }

    /// Carries the vendor's own label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Carries what the vendor says it means.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// This choice with its id bounded and its labels cut to fit.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when the id does not survive bounding.
    pub fn normalized(self) -> Result<Self> {
        Ok(Self {
            id: QuestionOptionId::new(normalize::opaque_id(
                self.id.as_str(),
                "question option id",
            )?),
            label: self
                .label
                .map(|label| normalize::bound_text(&label, TextLimit::ApprovalOptionLabel).text),
            description: self
                .description
                .map(|text| normalize::bound_text(&text, TextLimit::Detail).text),
        })
    }
}

/// What kind of answer one question takes.
///
/// There is no secret arm and no form arm. A vendor asking for either is refused with
/// [`UnsupportedQuestion`], never reshaped into [`QuestionForm::FreeText`]: a password typed into
/// a box labelled "answer" is a password in a host's transcript, and an arbitrary form is a
/// rendering surface this library has not agreed to own.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum QuestionForm {
    /// Pick from the choices the vendor offered.
    Choice {
        /// The choices, in the vendor's own order.
        options: Vec<QuestionOption>,
        /// Whether more than one may be chosen.
        #[serde(default)]
        multi_select: bool,
    },
    /// Write something.
    FreeText {
        /// A hint the vendor wrote for the input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
}

/// One thing the vendor wants to know.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Question {
    /// The vendor's own id for this question, echoed back with its answer.
    pub id: QuestionId,
    /// What it is asking, as plain text.
    pub prompt: String,
    /// More, when the vendor said more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What kind of answer it takes.
    pub form: QuestionForm,
    /// Whether the vendor needs an answer to this one before it can go on.
    #[serde(default)]
    pub required: bool,
}

impl Question {
    /// A question with no detail and no requirement.
    pub fn new(id: QuestionId, prompt: impl Into<String>, form: QuestionForm) -> Self {
        Self {
            id,
            prompt: prompt.into(),
            detail: None,
            form,
            required: false,
        }
    }

    /// Carries more of what the vendor said.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Marks this one as needing an answer.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// This question with every vendor-written value bounded.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when the question id or an option id does not survive
    /// bounding, and [`Error::Protocol`] when a choice offers nothing or more than
    /// [`QUESTION_MAX_OPTIONS`] choices.
    pub fn normalized(self) -> Result<Self> {
        let form = match self.form {
            QuestionForm::Choice {
                options,
                multi_select,
            } => {
                if options.is_empty() || options.len() > QUESTION_MAX_OPTIONS {
                    return Err(Error::Protocol {
                        expected: format!("between 1 and {QUESTION_MAX_OPTIONS} question options"),
                        received: options.len().to_string(),
                    });
                }
                QuestionForm::Choice {
                    options: options
                        .into_iter()
                        .map(QuestionOption::normalized)
                        .collect::<Result<Vec<_>>>()?,
                    multi_select,
                }
            }
            QuestionForm::FreeText { placeholder } => QuestionForm::FreeText {
                placeholder: placeholder
                    .map(|text| normalize::bound_text(&text, TextLimit::Title).text),
            },
        };
        Ok(Self {
            id: QuestionId::new(normalize::opaque_id(self.id.as_str(), "question id")?),
            prompt: normalize::bound_text(&self.prompt, TextLimit::Detail).text,
            detail: self
                .detail
                .map(|detail| normalize::bound_text(&detail, TextLimit::Detail).text),
            form,
            required: self.required,
        })
    }
}

/// The vendor is asking for information.
///
/// Several questions at once, because a vendor that asks three things in one round trip is
/// answered in one round trip. Nothing here authorises anything; see
/// [`PermissionRequest`](crate::PermissionRequest) for the surface that does.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuestionRequest {
    /// The lifecycle fields: id, kind, session, turn, deadline, status.
    pub interaction: Interaction,
    /// A one-line summary of what the round of questions is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What it wants to know.
    pub questions: Vec<Question>,
    /// True when any field above was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl QuestionRequest {
    /// One round of questions.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     Interaction, InteractionId, InteractionKind, Question, QuestionForm, QuestionId,
    ///     QuestionRequest, SessionId,
    /// };
    /// use std::time::SystemTime;
    ///
    /// let request = QuestionRequest::new(
    ///     Interaction::new(
    ///         InteractionId::new("ask-1"),
    ///         InteractionKind::Question,
    ///         SessionId::new("chat-1"),
    ///         SystemTime::now(),
    ///     ),
    ///     vec![Question::new(
    ///         QuestionId::new("branch"),
    ///         "Which branch?",
    ///         QuestionForm::FreeText { placeholder: None },
    ///     )],
    /// );
    /// assert_eq!(request.questions.len(), 1);
    /// ```
    pub fn new(interaction: Interaction, questions: Vec<Question>) -> Self {
        Self {
            interaction,
            title: None,
            questions,
            truncated: false,
        }
    }

    /// Carries a summary of the round.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// One question, by the vendor's own id.
    pub fn question(&self, id: &QuestionId) -> Option<&Question> {
        self.questions.iter().find(|question| &question.id == id)
    }

    /// This request with every vendor-written value bounded.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the request carries no questions or more than
    /// [`QUESTION_MAX_ITEMS`], and [`Error::InvalidVendorValue`] when an id does not survive
    /// bounding. A round nobody can render is refused on its own rather than ending its turn.
    pub fn normalized(self) -> Result<Self> {
        if self.questions.is_empty() || self.questions.len() > QUESTION_MAX_ITEMS {
            return Err(Error::Protocol {
                expected: format!("between 1 and {QUESTION_MAX_ITEMS} questions"),
                received: self.questions.len().to_string(),
            });
        }
        let title = self
            .title
            .map(|title| normalize::bound_text(&title, TextLimit::Title));
        let truncated = self.truncated || title.as_ref().is_some_and(|title| title.truncated);
        Ok(Self {
            interaction: self.interaction.normalized()?,
            title: title.map(|title| title.text),
            questions: self
                .questions
                .into_iter()
                .map(Question::normalized)
                .collect::<Result<Vec<_>>>()?,
            truncated,
        })
    }

    /// Checks one answer set against the questions actually asked.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when an answer names a question this request did not ask, when a choice
    /// names an option it did not offer, when a single-select question is given several answers,
    /// or when a required question is left unanswered. A host cannot invent a choice: the vendor
    /// would refuse it, or worse, accept a different one.
    pub fn validate(&self, response: &QuestionResponse) -> Result<()> {
        if response.interaction_id != self.interaction.id {
            return Err(Error::Protocol {
                expected: format!("an answer to {}", self.interaction.id),
                received: response.interaction_id.to_string(),
            });
        }
        for answer in &response.answers {
            let Some(question) = self.question(&answer.question_id) else {
                return Err(Error::Protocol {
                    expected: format!("one of the questions {:?} this request asked", self.ids()),
                    received: answer.question_id.to_string(),
                });
            };
            validate_answer(question, answer)?;
        }
        for question in self.questions.iter().filter(|question| question.required) {
            let answered = response
                .answers
                .iter()
                .any(|answer| answer.question_id == question.id && !answer.value.is_declined());
            if !answered {
                return Err(Error::Protocol {
                    expected: format!("an answer to the required question {}", question.id),
                    received: String::from("no answer"),
                });
            }
        }
        Ok(())
    }

    fn ids(&self) -> Vec<&str> {
        self.questions
            .iter()
            .map(|question| question.id.as_str())
            .collect()
    }
}

/// Checks one answer against the shape of the question it answers.
fn validate_answer(question: &Question, answer: &Answer) -> Result<()> {
    match (&question.form, &answer.value) {
        (
            QuestionForm::Choice {
                options,
                multi_select,
            },
            AnswerValue::Chosen { option_ids },
        ) => {
            if option_ids.is_empty() {
                return Err(Error::Protocol {
                    expected: String::from("at least one chosen option"),
                    received: String::from("none"),
                });
            }
            if !multi_select && option_ids.len() > 1 {
                return Err(Error::Protocol {
                    expected: format!("one option for the single-select question {}", question.id),
                    received: option_ids.len().to_string(),
                });
            }
            for chosen in option_ids {
                if !options.iter().any(|option| &option.id == chosen) {
                    return Err(Error::Protocol {
                        expected: format!(
                            "one of the options {:?} question {} offered",
                            options
                                .iter()
                                .map(|option| option.id.as_str())
                                .collect::<Vec<_>>(),
                            question.id
                        ),
                        received: chosen.to_string(),
                    });
                }
            }
            Ok(())
        }
        (QuestionForm::FreeText { .. }, AnswerValue::Text { text }) => {
            if text.chars().count() > ANSWER_TEXT_MAX_LENGTH {
                return Err(Error::Protocol {
                    expected: format!("at most {ANSWER_TEXT_MAX_LENGTH} characters"),
                    received: text.chars().count().to_string(),
                });
            }
            Ok(())
        }
        (_, AnswerValue::Declined) => Ok(()),
        (form, value) => Err(Error::Protocol {
            expected: format!("an answer matching {form:?}"),
            received: format!("{value:?}"),
        }),
    }
}

/// What one question was answered with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AnswerValue {
    /// These options, by the vendor's own ids.
    Chosen {
        /// What was chosen, in the order it was chosen.
        option_ids: Vec<QuestionOptionId>,
    },
    /// This text.
    Text {
        /// What was written.
        text: String,
    },
    /// Nothing; the person would rather not say.
    ///
    /// Distinct from empty text, which is an answer of "nothing".
    Declined,
}

impl AnswerValue {
    /// One chosen option.
    pub fn chosen(option_id: QuestionOptionId) -> Self {
        Self::Chosen {
            option_ids: vec![option_id],
        }
    }

    /// Free text.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Whether this answer says nothing.
    pub const fn is_declined(&self) -> bool {
        matches!(self, Self::Declined)
    }
}

/// One question's answer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Answer {
    /// Which question, by the vendor's own id.
    pub question_id: QuestionId,
    /// What it was answered with.
    pub value: AnswerValue,
}

impl Answer {
    /// One answer to one question.
    pub fn new(question_id: QuestionId, value: AnswerValue) -> Self {
        Self { question_id, value }
    }
}

/// The answers to one round of questions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuestionResponse {
    /// Which round this answers.
    pub interaction_id: InteractionId,
    /// The answers, in any order.
    pub answers: Vec<Answer>,
}

impl QuestionResponse {
    /// The answers to one round.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     Answer, AnswerValue, InteractionId, QuestionId, QuestionResponse,
    /// };
    ///
    /// let response = QuestionResponse::new(
    ///     InteractionId::new("ask-1"),
    ///     vec![Answer::new(QuestionId::new("branch"), AnswerValue::text("main"))],
    /// );
    /// assert_eq!(response.answers.len(), 1);
    /// ```
    pub fn new(interaction_id: InteractionId, answers: Vec<Answer>) -> Self {
        Self {
            interaction_id,
            answers,
        }
    }
}

/// Why a vendor's question was not put to a host.
///
/// A refusal by name, because the alternative — reshaping the ask into something this library does
/// carry — is how a credential prompt becomes an ordinary text field in a transcript.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum UnsupportedQuestion {
    /// The vendor asked for a credential, a password or a token.
    ///
    /// Outside approved scope, in either direction: this library never collects a secret and never
    /// forwards one.
    SecretCollection,
    /// The vendor asked for an arbitrary form this library does not render.
    ArbitraryForm,
    /// The vendor asked in a shape no arm of [`QuestionForm`] describes.
    UnrecognisedForm {
        /// What the vendor called it, bounded.
        received: String,
    },
}

impl UnsupportedQuestion {
    /// This reason with its vendor-written label bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::UnrecognisedForm { received } => Self::UnrecognisedForm {
                received: normalize::bound_text(&received, TextLimit::Title).text,
            },
            known => known,
        }
    }
}

impl fmt::Display for UnsupportedQuestion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SecretCollection => formatter.write_str("secret collection"),
            Self::ArbitraryForm => formatter.write_str("an arbitrary form"),
            Self::UnrecognisedForm { received } => {
                write!(formatter, "an unrecognised form {received:?}")
            }
        }
    }
}

/// How one round of questions ended.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum QuestionOutcome {
    /// Somebody answered.
    Answered {
        /// What they said.
        answers: Vec<Answer>,
    },
    /// Nobody answered in time.
    Expired,
    /// The turn or session ended first.
    Cancelled,
    /// The library would not put it to a host, and this is why.
    Refused {
        /// Why not.
        reason: UnsupportedQuestion,
    },
}

impl QuestionOutcome {
    /// The status this outcome leaves its interaction in.
    pub const fn status(&self) -> InteractionStatus {
        match self {
            Self::Answered { .. } => InteractionStatus::Resolved,
            Self::Expired => InteractionStatus::Expired,
            Self::Cancelled => InteractionStatus::Cancelled,
            Self::Refused { .. } => InteractionStatus::Withdrawn,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Answer, AnswerValue, Interaction, InteractionId, InteractionKind, InteractionStatus,
        Question, QuestionForm, QuestionId, QuestionOption, QuestionOptionId, QuestionOutcome,
        QuestionRequest, QuestionResponse, UnsupportedQuestion,
    };
    use crate::event::SessionId;
    use std::time::{Duration, SystemTime};

    fn interaction() -> Interaction {
        Interaction::new(
            InteractionId::new("ask-1"),
            InteractionKind::Question,
            SessionId::new("chat-1"),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
    }

    fn choice(id: &str, options: &[&str], multi_select: bool) -> Question {
        Question::new(
            QuestionId::new(id),
            "pick one",
            QuestionForm::Choice {
                options: options
                    .iter()
                    .map(|option| QuestionOption::new(QuestionOptionId::new(*option)))
                    .collect(),
                multi_select,
            },
        )
    }

    fn free_text(id: &str) -> Question {
        Question::new(
            QuestionId::new(id),
            "say something",
            QuestionForm::FreeText { placeholder: None },
        )
    }

    #[test]
    fn several_questions_and_their_answers_round_trip_with_the_vendors_own_ids() {
        let request = QuestionRequest::new(
            interaction(),
            vec![
                choice("branch", &["main", "next"], false),
                free_text("note"),
            ],
        )
        .with_title("Before I start")
        .normalized()
        .expect("expected a bounded request");

        let response = QuestionResponse::new(
            request.interaction.id.clone(),
            vec![
                Answer::new(
                    QuestionId::new("branch"),
                    AnswerValue::chosen(QuestionOptionId::new("next")),
                ),
                Answer::new(QuestionId::new("note"), AnswerValue::text("ship it")),
            ],
        );
        request
            .validate(&response)
            .expect("expected the answers to be accepted");

        let encoded = serde_json::to_value(&response).expect("expected a serializable response");
        assert_eq!(encoded["interactionId"], "ask-1");
        assert_eq!(encoded["answers"][0]["questionId"], "branch");
        assert_eq!(encoded["answers"][0]["value"]["option_ids"][0], "next");
        assert_eq!(encoded["answers"][1]["value"]["text"], "ship it");
        assert_eq!(
            serde_json::from_value::<QuestionResponse>(encoded)
                .expect("expected the response back"),
            response
        );
    }

    /// A host cannot invent a choice: the vendor would refuse it, or worse, accept a different one.
    #[test]
    fn an_option_the_question_did_not_offer_is_refused_by_name() {
        let request = QuestionRequest::new(interaction(), vec![choice("branch", &["main"], false)]);
        let error = request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-1"),
                vec![Answer::new(
                    QuestionId::new("branch"),
                    AnswerValue::chosen(QuestionOptionId::new("trunk")),
                )],
            ))
            .expect_err("expected a refusal, received acceptance");
        assert!(
            error.to_string().contains("trunk"),
            "expected the rejected option in the diagnostic, received {error}"
        );
    }

    #[test]
    fn a_single_select_question_refuses_more_than_one_answer() {
        let request = QuestionRequest::new(
            interaction(),
            vec![choice("branch", &["main", "next"], false)],
        );
        let error = request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-1"),
                vec![Answer::new(
                    QuestionId::new("branch"),
                    AnswerValue::Chosen {
                        option_ids: vec![
                            QuestionOptionId::new("main"),
                            QuestionOptionId::new("next"),
                        ],
                    },
                )],
            ))
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("single-select"),
            "received {error}"
        );
    }

    #[test]
    fn a_multi_select_question_accepts_several() {
        let request =
            QuestionRequest::new(interaction(), vec![choice("targets", &["a", "b"], true)]);
        request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-1"),
                vec![Answer::new(
                    QuestionId::new("targets"),
                    AnswerValue::Chosen {
                        option_ids: vec![QuestionOptionId::new("a"), QuestionOptionId::new("b")],
                    },
                )],
            ))
            .expect("expected several answers to be accepted");
    }

    #[test]
    fn a_required_question_left_unanswered_is_refused() {
        let request = QuestionRequest::new(
            interaction(),
            vec![free_text("note").required(), free_text("extra")],
        );
        let error = request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-1"),
                vec![Answer::new(
                    QuestionId::new("extra"),
                    AnswerValue::text("x"),
                )],
            ))
            .expect_err("expected a refusal");
        assert!(error.to_string().contains("note"), "received {error}");
    }

    /// Declining is an answer that says nothing, and it does not satisfy a required question.
    #[test]
    fn declining_a_required_question_is_not_answering_it() {
        let request = QuestionRequest::new(interaction(), vec![free_text("note").required()]);
        let error = request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-1"),
                vec![Answer::new(QuestionId::new("note"), AnswerValue::Declined)],
            ))
            .expect_err("expected a refusal");
        assert!(error.to_string().contains("required"), "received {error}");
    }

    #[test]
    fn an_answer_to_another_round_is_refused() {
        let request = QuestionRequest::new(interaction(), vec![free_text("note")]);
        let error = request
            .validate(&QuestionResponse::new(
                InteractionId::new("ask-2"),
                Vec::new(),
            ))
            .expect_err("expected a refusal");
        assert!(error.to_string().contains("ask-2"), "received {error}");
    }

    /// Answering a question grants nothing. The distinction is what stops a "which branch?" prompt
    /// being recorded as, and policy-matched against, an executable grant.
    #[test]
    fn a_question_never_grants_authority_and_a_permission_always_can() {
        assert!(!InteractionKind::Question.grants_authority());
        assert!(InteractionKind::Permission.grants_authority());
    }

    #[test]
    fn a_round_with_no_questions_is_refused_rather_than_rendered_empty() {
        let error = QuestionRequest::new(interaction(), Vec::new())
            .normalized()
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("between 1 and"),
            "received {error}"
        );
    }

    #[test]
    fn a_question_id_that_cannot_survive_bounding_is_refused_rather_than_cut() {
        let error = QuestionRequest::new(interaction(), vec![free_text(&"q".repeat(129))])
            .normalized()
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("question id"),
            "received {error}"
        );
    }

    /// A secret prompt is refused by name rather than reshaped into ordinary free text.
    #[test]
    fn an_unsupported_ask_is_withdrawn_under_its_own_reason() {
        let outcome = QuestionOutcome::Refused {
            reason: UnsupportedQuestion::SecretCollection,
        };
        assert_eq!(outcome.status(), InteractionStatus::Withdrawn);
        assert_eq!(
            UnsupportedQuestion::SecretCollection.to_string(),
            "secret collection"
        );
    }

    #[test]
    fn every_outcome_maps_to_the_status_it_leaves_behind() {
        assert_eq!(
            QuestionOutcome::Answered {
                answers: Vec::new()
            }
            .status(),
            InteractionStatus::Resolved
        );
        assert_eq!(
            QuestionOutcome::Expired.status(),
            InteractionStatus::Expired
        );
        assert_eq!(
            QuestionOutcome::Cancelled.status(),
            InteractionStatus::Cancelled
        );
    }

    #[test]
    fn an_interaction_carries_the_turn_and_attempt_that_owns_it() {
        use crate::event::TurnId;
        use crate::operation::{AttemptId, OperationRef};

        let owned = interaction().during(OperationRef::new(
            SessionId::new("chat-1"),
            TurnId::new("turn-1"),
            AttemptId::new("attempt-2"),
        ));
        assert_eq!(
            owned
                .operation
                .as_ref()
                .map(|operation| operation.attempt.as_str()),
            Some("attempt-2")
        );
        assert!(owned.is_open());
        assert!(!owned.resolved_as(InteractionStatus::Expired).is_open());
    }
}
