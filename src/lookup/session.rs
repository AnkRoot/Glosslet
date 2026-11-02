//! 查词状态机：当前意图、原文寿命、绘制确认和重试归属在这里闭合。

use crate::core::{
    Content, Display, DisplayIdentity, Failure, MAX_SELECTION_BYTES, Request, Settings, Ticket,
    Translation,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const FRAME_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug)]
enum Pending {
    Capture { ticket: Ticket, settings: Settings },
    AwaitingFrame { request: Request, deadline: Instant },
    Translation(Request),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureResult {
    Ignored,
    Empty,
    Failed,
    Ready,
}

#[derive(Default, Debug)]
pub(super) struct Session {
    next: u64,
    pending: Option<Pending>,
    display: Option<Display>,
    pinned: bool,
}

impl Session {
    fn ticket(&mut self) -> Ticket {
        self.next += 1;
        Ticket(self.next)
    }

    pub(super) fn phase(&self) -> &'static str {
        match &self.pending {
            Some(Pending::Capture { .. }) => "取词中",
            Some(Pending::AwaitingFrame { .. }) => "等待加载帧",
            Some(Pending::Translation(_)) => "翻译中",
            None => match self.display.as_ref().map(|display| &display.content) {
                Some(Content::Ready(Translation::Dictionary(_))) => "结果（词典）",
                Some(Content::Ready(Translation::Sentence(_))) => "结果（句子）",
                Some(Content::Ready(Translation::NoResult)) => "结果（无结果）",
                Some(Content::Failed { .. }) => "失败",
                Some(Content::Loading) => "等待加载帧",
                None => "空闲",
            },
        }
    }
    pub(super) fn display(&self) -> Option<&Display> {
        self.display.as_ref()
    }
    pub(super) fn pinned(&self) -> bool {
        self.pinned
    }
    pub(super) fn capture_ticket(&self) -> Option<Ticket> {
        match self.pending {
            Some(Pending::Capture { ticket, .. }) => Some(ticket),
            _ => None,
        }
    }
    pub(super) fn capturing(&self) -> bool {
        self.capture_ticket().is_some()
    }

    pub(super) fn capture(&mut self, settings: Settings) -> Ticket {
        let ticket = self.ticket();
        self.pending = Some(Pending::Capture { ticket, settings });
        ticket
    }

    pub(super) fn captured(
        &mut self,
        ticket: Ticket,
        result: Result<String, Failure>,
    ) -> CaptureResult {
        let Some(Pending::Capture {
            ticket: expected,
            settings,
        }) = &self.pending
        else {
            return CaptureResult::Ignored;
        };
        if ticket != *expected {
            return CaptureResult::Ignored;
        }
        let settings = *settings;
        let result = result.and_then(|source| {
            if source.len() > MAX_SELECTION_BYTES {
                Err(Failure::SelectionTooLarge)
            } else {
                Ok(source)
            }
        });
        match result {
            Ok(source) if source.trim().is_empty() => {
                self.cancel_capture();
                CaptureResult::Empty
            }
            Ok(source) => {
                self.wait_for_frame(Request {
                    ticket,
                    source: Arc::from(source.trim()),
                    settings,
                });
                CaptureResult::Ready
            }
            Err(error) => {
                self.pending = None;
                self.display = Some(Display::new(ticket, Content::Failed { error, retry: None }));
                CaptureResult::Failed
            }
        }
    }

    fn wait_for_frame(&mut self, request: Request) {
        self.display = Some(Display::new(request.ticket, Content::Loading));
        self.pending = Some(Pending::AwaitingFrame {
            request,
            deadline: Instant::now() + FRAME_TIMEOUT,
        });
    }

    pub(super) fn ready_deadline(&self) -> Option<Instant> {
        match &self.pending {
            Some(Pending::AwaitingFrame { deadline, .. }) => Some(*deadline),
            _ => None,
        }
    }

    pub(super) fn start_translation(
        &mut self,
        painted: DisplayIdentity,
        now: Instant,
    ) -> Option<Request> {
        let Some(Pending::AwaitingFrame { request, deadline }) = &self.pending else {
            return None;
        };
        if request.ticket != painted.ticket
            || now >= *deadline
            || !matches!(self.content(painted), Some(Content::Loading))
        {
            return None;
        }
        let Some(Pending::AwaitingFrame { request, .. }) = self.pending.take() else {
            return None;
        };
        self.pending = Some(Pending::Translation(request.clone()));
        Some(request)
    }

    pub(super) fn translated(
        &mut self,
        ticket: Ticket,
        result: Result<Translation, Failure>,
    ) -> bool {
        if !matches!(&self.pending, Some(Pending::Translation(request)) if request.ticket == ticket)
        {
            return false;
        }
        let Some(display) = self
            .display
            .as_mut()
            .filter(|display| display.ticket == ticket)
        else {
            return false;
        };
        let Some(Pending::Translation(request)) = self.pending.take() else {
            return false;
        };
        display.replace(match result {
            Ok(result) => Content::Ready(result),
            Err(error) => Content::Failed {
                error,
                retry: Some(request),
            },
        });
        true
    }

    pub(super) fn content(&self, identity: DisplayIdentity) -> Option<&Content> {
        let display = self.display.as_ref()?;
        if display.identity() != identity || self.capturing() {
            return None;
        }
        Some(&display.content)
    }

    pub(super) fn retry(&mut self, painted: DisplayIdentity) -> Option<Ticket> {
        let Content::Failed {
            retry: Some(request),
            ..
        } = self.content(painted)?
        else {
            return None;
        };
        let mut request = request.clone();
        request.ticket = self.ticket();
        let ticket = request.ticket;
        self.wait_for_frame(request);
        Some(ticket)
    }

    pub(super) fn cancel_capture(&mut self) -> bool {
        if !self.capturing() {
            return false;
        }
        self.pending = None;
        if self
            .display
            .as_ref()
            .is_some_and(|display| matches!(display.content, Content::Loading))
        {
            if self.pinned {
                if let Some(display) = &mut self.display {
                    display.replace(Content::Ready(Translation::NoResult));
                }
            } else {
                self.display = None;
            }
        }
        true
    }

    pub(super) fn set_pinned(&mut self, pinned: bool) -> bool {
        if self.display.is_none() || self.capturing() {
            return false;
        }
        let changed = self.pinned != pinned;
        self.pinned = pinned;
        changed
    }

    pub(super) fn probe_panel(&mut self) -> Ticket {
        self.close();
        let ticket = self.ticket();
        self.display = Some(Display::new(
            ticket,
            Content::Ready(Translation::Sentence(
                "Glosslet 面板测试\n不读取或发送选区".into(),
            )),
        ));
        ticket
    }

    pub(super) fn close(&mut self) {
        self.pending = None;
        self.display = None;
        self.pinned = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(session: &mut Session, text: &str) -> DisplayIdentity {
        let ticket = session.capture(Settings::default());
        assert_eq!(
            session.captured(ticket, Ok(text.into())),
            CaptureResult::Ready
        );
        session.display().unwrap().identity()
    }

    fn running(session: &mut Session, text: &str) -> Request {
        let frame = ready(session, text);
        session.start_translation(frame, Instant::now()).unwrap()
    }

    #[test]
    fn only_the_latest_intent_can_capture_start_or_complete() {
        let mut session = Session::default();
        let replaced = session.capture(Settings::default());
        let current = ready(&mut session, "current");
        assert_eq!(
            session.captured(replaced, Ok("old".into())),
            CaptureResult::Ignored
        );
        assert!(!session.translated(current.ticket, Ok(Translation::NoResult)));
        let request = session.start_translation(current, Instant::now()).unwrap();
        assert!(session.start_translation(current, Instant::now()).is_none());
        session.close();
        let new = ready(&mut session, "new");
        assert!(!session.translated(request.ticket, Ok(Translation::NoResult)));
        assert!(session.start_translation(current, Instant::now()).is_none());
        assert!(session.start_translation(new, Instant::now()).is_some());
    }

    #[test]
    fn retry_waits_for_its_own_loading_frame_and_cannot_start_at_or_after_deadline() {
        for timing in [-1, 0, 1] {
            let mut session = Session::default();
            let request = running(&mut session, "source");
            assert!(session.translated(request.ticket, Err(Failure::Network)));
            let failed = session.display().unwrap().identity();
            session.set_pinned(true);
            let retry = session.retry(failed).unwrap();
            let loading = session.display().unwrap().identity();
            let deadline = session.ready_deadline().unwrap();
            assert!(
                session
                    .start_translation(failed, deadline - Duration::from_nanos(1))
                    .is_none()
            );
            let now = match timing {
                -1 => deadline - Duration::from_nanos(1),
                0 => deadline,
                _ => deadline + Duration::from_nanos(1),
            };
            let started = session.start_translation(loading, now);
            assert!(session.pinned());
            if timing < 0 {
                assert_eq!(started.unwrap().ticket, retry);
            } else {
                assert!(started.is_none());
            }
        }
    }

    #[test]
    fn an_empty_or_cancelled_capture_preserves_ready_content_not_stale_loading() {
        let mut session = Session::default();
        let request = running(&mut session, "source");
        session.translated(request.ticket, Ok(Translation::Sentence("result".into())));
        let frame = session.display().unwrap().identity();
        let empty = session.capture(Settings::default());
        assert!(session.content(frame).is_none());
        assert_eq!(
            session.captured(empty, Ok(" \n".into())),
            CaptureResult::Empty
        );
        assert!(
            matches!(session.content(frame), Some(Content::Ready(Translation::Sentence(text))) if text == "result")
        );
        for pinned in [false, true] {
            let mut session = Session::default();
            let old = running(&mut session, "old");
            session.set_pinned(pinned);
            let loading = session.display().unwrap().identity();
            session.capture(Settings::default());
            assert!(session.cancel_capture());
            assert!(!session.translated(old.ticket, Ok(Translation::NoResult)));
            assert!(session.content(loading).is_none());
            assert!(
                !session
                    .display()
                    .is_some_and(|display| matches!(display.content, Content::Loading))
            );
            assert_eq!(session.pinned(), pinned);
        }
    }

    #[test]
    fn retry_is_bound_to_current_failure_and_original_settings() {
        let mut session = Session::default();
        let settings = Settings {
            service: crate::core::Service::Google,
            direction: crate::core::Direction::Japanese,
            ..Settings::default()
        };
        let ticket = session.capture(settings);
        session.captured(ticket, Ok("original".into()));
        let frame = session.display().unwrap().identity();
        let request = session.start_translation(frame, Instant::now()).unwrap();
        session.translated(ticket, Err(Failure::RateLimited));
        let failure = session.display().unwrap().identity();
        assert!(session.retry(frame).is_none());
        let retry = session.retry(failure).unwrap();
        let frame = session.display().unwrap().identity();
        assert!(session.retry(failure).is_none());
        let restarted = session.start_translation(frame, Instant::now()).unwrap();
        assert_ne!(retry, ticket);
        assert_eq!(restarted.source, request.source);
        assert_eq!(restarted.settings, settings);
        assert!(!session.translated(ticket, Ok(Translation::NoResult)));
        assert!(session.translated(retry, Ok(Translation::NoResult)));
    }

    #[test]
    fn completions_cannot_authorize_actions_from_the_old_loading_frame() {
        for result in [
            Ok(Translation::Sentence("result".into())),
            Err(Failure::Network),
        ] {
            let mut session = Session::default();
            let request = running(&mut session, "source");
            let loading = session.display().unwrap().identity();
            session.translated(request.ticket, result);
            assert!(session.content(loading).is_none());
            assert!(session.retry(loading).is_none());
            assert!(
                session
                    .content(session.display().unwrap().identity())
                    .is_some()
            );
        }
    }

    #[test]
    fn capture_validation_and_resource_release_do_not_depend_on_network() {
        let mut session = Session::default();
        let ticket = session.capture(Settings::default());
        assert_eq!(
            session.captured(ticket, Ok("a".repeat(MAX_SELECTION_BYTES + 1))),
            CaptureResult::Failed
        );
        assert!(matches!(
            session.display().unwrap().content,
            Content::Failed {
                error: Failure::SelectionTooLarge,
                retry: None
            }
        ));
        let request = running(&mut session, "private sample");
        let source = Arc::downgrade(&request.source);
        drop(request);
        session.translated(session.display().unwrap().ticket, Err(Failure::Network));
        assert!(source.upgrade().is_some());
        session.close();
        assert!(source.upgrade().is_none());
    }

    #[test]
    fn hidden_or_capturing_sessions_cannot_change_pin_state() {
        let mut session = Session::default();
        assert!(!session.set_pinned(true));
        assert!(!session.pinned());
        session.probe_panel();
        assert!(session.set_pinned(true));
        let ticket = session.capture(Settings::default());
        assert!(!session.set_pinned(false));
        assert!(session.pinned());
        assert_eq!(session.capture_ticket(), Some(ticket));
        assert!(session.cancel_capture());
        assert!(session.set_pinned(false));
        assert!(session.set_pinned(true));
        session.close();
        assert!(!session.pinned());
        assert!(!session.set_pinned(true));
    }

    #[test]
    fn pinning_neither_changes_content_nor_creates_lookup_work() {
        let mut session = Session::default();
        let request = running(&mut session, "source");
        session.translated(request.ticket, Ok(Translation::NoResult));
        let frame = session.display().unwrap().identity();
        assert!(session.set_pinned(true));
        assert!(!session.set_pinned(true));
        assert!(session.set_pinned(false));
        assert!(!session.set_pinned(false));
        assert_eq!(session.display().unwrap().identity(), frame);
        assert!(session.ready_deadline().is_none());
        assert!(session.start_translation(frame, Instant::now()).is_none());
    }
}
