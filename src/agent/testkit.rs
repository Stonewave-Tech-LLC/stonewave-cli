//! Stand-ins for the network, the screen and the clock, shared by the tests of the agent commands.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use super::ui::{Kind, Ui};
use crate::connect::{ConnectError, Http, Pace, Reply};
use crate::term;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub url: String,
    pub bearer: Option<String>,
    pub body: Option<String>,
}

/// Scripted answers in order; remembers the requests.
pub struct FakeHttp {
    replies: RefCell<VecDeque<Result<Reply, ConnectError>>>,
    seen: RefCell<Vec<Seen>>,
}

pub fn reply(status: u16, body: &str) -> Result<Reply, ConnectError> {
    Ok(Reply {
        status,
        body: body.into(),
        retry_after: None,
    })
}

impl FakeHttp {
    pub fn new(replies: Vec<Result<Reply, ConnectError>>) -> FakeHttp {
        FakeHttp {
            replies: RefCell::new(replies.into()),
            seen: RefCell::new(Vec::new()),
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.borrow().clone()
    }
}

impl Http for FakeHttp {
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&str>,
    ) -> Result<Reply, ConnectError> {
        self.seen.borrow_mut().push(Seen {
            method: method.into(),
            url: url.into(),
            bearer: bearer.map(Into::into),
            body: body.map(Into::into),
        });
        self.replies
            .borrow_mut()
            .pop_front()
            .expect("unexpected request")
    }
}

/// The same fake for the account and for Wave.
#[derive(Clone)]
pub struct Shared(pub Rc<FakeHttp>);

impl Http for Shared {
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&str>,
    ) -> Result<Reply, ConnectError> {
        self.0.send(method, url, bearer, body)
    }
}

/// A virtual clock: each tick of the sign-in wait and each pause moves it.
pub struct FakePace(pub Rc<Cell<Duration>>);

impl Pace for FakePace {
    fn elapsed(&self) -> Duration {
        self.0.get()
    }

    fn tick(&mut self, _: Duration) -> Result<(), term::Error> {
        self.0.set(self.0.get() + Duration::from_millis(100));
        Ok(())
    }
}

/// Records what was said, answers questions from a list.
pub struct TestUi {
    pub events: Vec<(Kind, String, String)>,
    pub answers: VecDeque<bool>,
    pub asked: Vec<String>,
    pub pauses: Vec<(Duration, String)>,
    pub clock: Rc<Cell<Duration>>,
    pub can_ask: bool,
    /// When set, a pause fails like Ctrl-C.
    pub interrupt_on_pause: bool,
}

impl TestUi {
    pub fn new(answers: &[bool]) -> TestUi {
        TestUi {
            events: Vec::new(),
            answers: answers.iter().copied().collect(),
            asked: Vec::new(),
            pauses: Vec::new(),
            clock: Rc::new(Cell::new(Duration::ZERO)),
            can_ask: true,
            interrupt_on_pause: false,
        }
    }

    /// Everything said, one line each.
    pub fn text(&self) -> String {
        self.events
            .iter()
            .map(|(k, a, b)| format!("{k:?}: {a} {b}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn has(&self, kind: Kind, needle: &str) -> bool {
        self.events
            .iter()
            .any(|(k, a, b)| *k == kind && (a.contains(needle) || b.contains(needle)))
    }
}

impl Ui for TestUi {
    fn emit(&mut self, kind: Kind, a: &str, b: &str) {
        self.events.push((kind, a.to_owned(), b.to_owned()));
    }

    fn ask_raw(&mut self, question: &str) -> term::Result<bool> {
        self.asked.push(question.to_owned());
        Ok(self.answers.pop_front().unwrap_or(false))
    }

    fn pause(&mut self, d: Duration, msg: &str) -> term::Result<()> {
        if self.interrupt_on_pause {
            return Err(term::Error::Interrupted);
        }
        self.pauses.push((d, msg.to_owned()));
        self.clock.set(self.clock.get() + d);
        Ok(())
    }

    fn pace(&mut self) -> Box<dyn Pace + '_> {
        Box::new(FakePace(self.clock.clone()))
    }

    fn banner(&mut self) -> term::Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> term::Result<()> {
        Ok(())
    }

    fn mark(&self) -> usize {
        0
    }

    fn finish_wait(&mut self, _mark: usize) {}

    fn can_ask(&self) -> bool {
        self.can_ask
    }
}
