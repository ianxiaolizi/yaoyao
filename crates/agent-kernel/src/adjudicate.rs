//! The rule layer that decides what a direct user message asks for.
//!
//! The 2026-11-01 incident had one shape: a new instruction arrived, the queue
//! still held the old one, and the scheduler answered the old one. Reading the
//! log afterwards could not say which request an answer belonged to. The intent
//! ledger (slice A1) made each request nameable; this layer is what reads a new
//! request against the queue and decides whether it *stands beside* the work in
//! flight or *overtakes* it.
//!
//! Two rules shape everything here:
//!
//! * **Rules first, models never.** "继续", "停止", "cancel" are not semantic
//!   problems to be sent to a provider — they are a vocabulary, and answering
//!   them locally is both cheaper and more predictable. Most sessions never
//!   reach a model for this decision at all.
//! * **Only an overtaking instruction displaces anything.** Displacement is
//!   destructive (the old item is discarded), so the classifier has to *know*
//!   the new message replaces the old one. Where it cannot tell, it says
//!   [`Route::Unclear`], which queues the message without touching what is
//!   already queued. A later slice (A3) asks a model about exactly those; until
//!   it exists, doubt costs a redundant turn rather than lost work.
//!
//! The unit is one message, not one session, and nothing here writes: the
//! caller folds the log, calls [`route`], and records what it decided.

use agent_protocol::intent::IntentKind;

/// What a direct user message asks the kernel to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Keep going on the work in flight: "继续", "可以", "好的".
    ///
    /// The message still opens a turn of its own — it is a thing the person
    /// said and deserves an answer — but it does not take anything out of the
    /// queue.
    Continue,
    /// A fresh instruction that stands on its own. It displaces every unclaimed
    /// instruction queued before it (`intent/superseded`), because a person who
    /// asks for something new has said what they want done next.
    NewTask,
    /// Withdraw what is queued or running: "停止", "取消", "暂停".
    ///
    /// Nothing is queued as a turn; queued instructions are withdrawn and, when
    /// something is running, an interrupt asks the run to stop at its next step
    /// boundary. Work already done keeps its evidence and artifacts.
    Stop,
    /// Nothing was said. An empty message asks for nothing and is refused by
    /// the caller rather than turned into a turn.
    NoRequest,
    /// The words could mean any of the above: a control word is present but not
    /// where a control word goes ("我觉得应该停止了"). Queued like a new
    /// instruction, displaces nothing, and is the one shape slice A3 may hand
    /// to an adjudicator model.
    Unclear,
}

impl Route {
    /// The wire spelling, recorded in the queued item's payload.
    ///
    /// Written down rather than re-derived at read time because the rule that
    /// produced it is a function of the queue *as it was when the message
    /// arrived*, and a later reader looking at a different queue would answer a
    /// different question. Slice A3 reads this to find the messages the rules
    /// could not route; nothing else should have to guess.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::NewTask => "new_task",
            Self::Stop => "stop",
            Self::NoRequest => "no_request",
            Self::Unclear => "unclear",
        }
    }

    /// The kind recorded beside the instruction this route describes.
    ///
    /// `Unclear` records [`IntentKind::NewTask`] because that is the queue item
    /// it produces and the write path cannot assert a relation it does not
    /// know; the *displacement* decision is this layer's, and it is recorded by
    /// whether `intent/superseded` events follow — not by the kind.
    pub fn intent_kind(self) -> Option<IntentKind> {
        match self {
            Self::Continue => Some(IntentKind::Continuation),
            Self::NewTask | Self::Unclear => Some(IntentKind::NewTask),
            Self::Stop => Some(IntentKind::Interrupt),
            // Nothing was asked, so there is nothing to record as asked.
            Self::NoRequest => None,
        }
    }

    /// Whether this message opens a turn of its own.
    pub fn queues_a_turn(self) -> bool {
        !matches!(self, Self::Stop | Self::NoRequest)
    }

    /// Whether this message displaces the instructions queued before it.
    pub fn displaces_pending(self) -> bool {
        matches!(self, Self::NewTask)
    }
}

/// What the classifier may look at besides the words.
///
/// Only one fact so far, and it is the only one that matters to the vocabulary:
/// whether there is work an acknowledgement could be acknowledging. "好的" is
/// a continuation while a turn is in flight and a plain first message when
/// nothing has started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteState {
    /// An open run, a held turn, or an instruction waiting in the queue.
    pub has_work: bool,
}

/// Classify one direct user message.
///
/// The text is normalized first (whitespace, surrounding punctuation, a
/// trailing particle such as 吧/啊, ASCII case), because a person typing into a
/// chat box does not type their control words in a canonical form.
pub fn route(text: &str, state: RouteState) -> Route {
    let normalized = normalize(text);
    if normalized.is_empty() {
        return Route::NoRequest;
    }

    // A control word, exactly. This is the case the vocabulary exists for and
    // the one that must never reach a model.
    if STOP_EXACT.contains(&normalized.as_str()) {
        return Route::Stop;
    }
    if CONTINUE_EXACT.contains(&normalized.as_str()) {
        return Route::Continue;
    }
    // An acknowledgement means "keep going" only when something is going.
    if ACK_EXACT.contains(&normalized.as_str()) {
        return if state.has_work { Route::Continue } else { Route::NewTask };
    }

    // A control word that opens the sentence carries the whole sentence: "继续
    // 把测试跑完" is a continuation of the work in flight, "取消前一个" is a
    // stop. Only the unambiguous multi-character forms are prefixes — "停" as a
    // prefix would make "停车怎么办" a stop.
    if STOP_PREFIXES.iter().any(|token| normalized.starts_with(token)) {
        return Route::Stop;
    }
    if CONTINUE_PREFIXES.iter().any(|token| normalized.starts_with(token)) {
        return Route::Continue;
    }
    // An addition ("另外…", "还有…") is a constraint on the work in flight, not a
    // replacement for it. It is queued and displaces nothing.
    if ADDITIVE_PREFIXES.iter().any(|token| normalized.starts_with(token)) {
        return Route::Continue;
    }

    // A control word somewhere other than the front. Where it is decides what
    // the sentence does, and it is not at the front — so this layer cannot say.
    // Only the strong forms are searched for: "行", "好" and "可以" appear
    // inside ordinary sentences often enough to be noise.
    if STRONG_TOKENS.iter().any(|token| normalized.contains(token)) {
        return Route::Unclear;
    }

    Route::NewTask
}

/// Strip what a chat box adds and a classifier should not see.
///
/// Trailing particles ("继续吧", "停下啊") are part of how the word is spoken,
/// not part of which word it is; surrounding punctuation is the same. The
/// leading side only loses opening brackets and quotes: a leading particle can
/// be the whole difference between "嗯" (an acknowledgement) and "嗯…" (also
/// one), and nothing else leads a Chinese sentence that matters here.
fn normalize(text: &str) -> String {
    let trimmed = text.trim();
    let trimmed = trimmed.trim_matches(|c: char| {
        c.is_whitespace() || TRAILING_PUNCTUATION.contains(&c) || LEADING_PUNCTUATION.contains(&c)
    });
    let lowered = trimmed.to_lowercase();
    let mut word = lowered.as_str();
    for particle in PARTICLES {
        if let Some(stripped) = word.strip_suffix(particle) {
            word = stripped;
            break;
        }
    }
    word.trim().to_string()
}

const TRAILING_PUNCTUATION: &[char] =
    &['。', '，', '！', '？', '；', '：', '、', ',', '.', '!', '?', ';', ':', '…', '~', '～'];
const LEADING_PUNCTUATION: &[char] = &['（', '(', '「', '“', '"', '\''];
// `了` is deliberately absent: it is the difference between "不用了" and
// "不用", and only the first is something this vocabulary knows.
const PARTICLES: &[&str] = &["吧", "啊", "哦", "呀", "啦", "嘛", "哈"];

/// Control words that mean "carry on with what is in flight".
const CONTINUE_EXACT: &[&str] = &[
    "继续",
    "接着",
    "接着做",
    "继续做",
    "往下",
    "下一步",
    "继续吧",
    "continue",
    "go on",
    "keep going",
    "carry on",
    "proceed",
    "resume",
];

/// Acknowledgements: "keep going" only when there is something to keep going.
const ACK_EXACT: &[&str] = &[
    "好",
    "好的",
    "可以",
    "行",
    "嗯",
    "嗯嗯",
    "对的",
    "是的",
    "收到",
    "没问题",
    "ok",
    "okay",
    "yes",
    "y",
    "sure",
    "fine",
    "go",
];

/// Control words that mean "withdraw what is queued or running".
const STOP_EXACT: &[&str] = &[
    "停",
    "停下",
    "停止",
    "暂停",
    "暂停一下",
    "取消",
    "取消掉",
    "终止",
    "别做了",
    "不用了",
    "算了",
    "stop",
    "pause",
    "cancel",
    "abort",
    "halt",
    "quit",
];

/// Multi-character control words that carry a whole sentence when they lead it.
const STOP_PREFIXES: &[&str] =
    &["先暂停", "先停", "停止", "暂停", "取消", "终止", "stop", "pause", "cancel", "abort", "halt"];
const CONTINUE_PREFIXES: &[&str] =
    &["继续", "接着", "往下", "continue", "go on", "keep going", "carry on", "proceed", "resume"];

/// Openers that add to the work in flight instead of replacing it.
const ADDITIVE_PREFIXES: &[&str] = &[
    "另外",
    "还有",
    "顺便",
    "补充",
    "再加",
    "而且",
    "再注意",
    "also",
    "plus",
    "and also",
    "additionally",
    "in addition",
];

/// Control words strong enough to be recognized anywhere in a sentence.
const STRONG_TOKENS: &[&str] = &["继续", "接着", "停止", "暂停", "取消", "终止", "cancel", "stop"];

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> RouteState {
        RouteState { has_work: false }
    }

    fn working() -> RouteState {
        RouteState { has_work: true }
    }

    #[test]
    fn a_control_word_is_recognized_exactly_and_through_punctuation() {
        for text in ["继续", "继续。", "继续！", "继续吧", " 继续 ", "Continue", "go on!"]
        {
            assert_eq!(route(text, working()), Route::Continue, "{text:?}");
        }
        for text in ["停止", "停止。", "停下吧", "取消", "暂停", "取消掉", "STOP", "cancel."]
        {
            assert_eq!(route(text, working()), Route::Stop, "{text:?}");
        }
    }

    #[test]
    fn a_control_word_that_leads_the_sentence_carries_it() {
        assert_eq!(route("继续把测试跑完", working()), Route::Continue);
        assert_eq!(route("取消前一个任务", working()), Route::Stop);
        assert_eq!(route("先暂停这个", working()), Route::Stop);
    }

    #[test]
    fn an_acknowledgement_is_a_continuation_only_when_something_is_running() {
        for text in ["好的", "可以", "行", "嗯", "ok", "yes"] {
            assert_eq!(route(text, working()), Route::Continue, "{text:?}");
            assert_eq!(route(text, idle()), Route::NewTask, "{text:?}");
        }
    }

    #[test]
    fn an_addition_does_not_displace_what_is_running() {
        assert_eq!(route("另外注意不要改动 X", working()), Route::Continue);
        assert_eq!(route("sudo apt install 之前顺便跑一下测试", working()), Route::NewTask);
        assert_eq!(route("还有一件事：把日志也清理了", working()), Route::Continue);
    }

    #[test]
    fn a_control_word_in_the_middle_is_ambiguous_rather_than_guessed() {
        assert_eq!(route("我觉得应该停止了", working()), Route::Unclear);
        assert_eq!(route("先别取消，我看看", working()), Route::Unclear);
        assert_eq!(route("这个测试我们继续跑下去会不会太久", working()), Route::Unclear);
    }

    #[test]
    fn a_plain_sentence_is_a_new_task() {
        assert_eq!(route("修复 junction 逃逸", working()), Route::NewTask);
        assert_eq!(route("FIX THE JUNCTION ESCAPE", working()), Route::NewTask);
        // A word that merely *contains* a control word is not a control word.
        assert_eq!(route("这个不行", working()), Route::NewTask);
        assert_eq!(route("停车怎么办", working()), Route::NewTask);
    }

    #[test]
    fn an_empty_message_asks_for_nothing() {
        for text in ["", "   ", "\n\t", "。", "！"] {
            assert_eq!(route(text, working()), Route::NoRequest, "{text:?}");
        }
    }

    #[test]
    fn a_route_says_what_it_does_to_the_queue() {
        assert!(Route::NewTask.displaces_pending());
        assert!(!Route::Continue.displaces_pending());
        assert!(!Route::Unclear.displaces_pending());
        assert!(!Route::Stop.displaces_pending());

        assert!(Route::Continue.queues_a_turn());
        assert!(Route::NewTask.queues_a_turn());
        assert!(Route::Unclear.queues_a_turn());
        assert!(!Route::Stop.queues_a_turn());
        assert!(!Route::NoRequest.queues_a_turn());

        assert_eq!(Route::Continue.intent_kind(), Some(IntentKind::Continuation));
        assert_eq!(Route::Stop.intent_kind(), Some(IntentKind::Interrupt));
        assert_eq!(Route::NoRequest.intent_kind(), None);

        assert_eq!(Route::Unclear.as_str(), "unclear");
        assert_eq!(Route::NewTask.as_str(), "new_task");
    }
}
