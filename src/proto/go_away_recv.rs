#![deny(unused_must_use)]

use crate::frame;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

#[derive(Clone, Debug, Default)]
struct Core(Arc<Mutex<CoreInner>>);

// NOT Clone on purpose
#[derive(Debug)]
struct GoAwayRecvInner(Core);

#[derive(Clone, Debug)]
pub(crate) struct GoAwayRecv(Arc<GoAwayRecvInner>);

// NOT Clone on purpose
#[derive(Debug)]
pub(crate) struct GoAwayRecvConnHandle(Core);

#[derive(Debug)]
struct CoreInner {
    state: RecvState,
    wake_conn: Option<Waker>,
    wake_user: Option<Waker>,
}

/// Cyclical states: Empty -> Received -> Handled -> Pending -> Empty, repeat.
#[derive(Debug)]
enum RecvState {
    /// Empty means the GoAwayRecv is ready to receive a GOAWAY from the Connection
    Empty,

    /// A GOAWAY was `recv()`d from the Connection, it ready to be given to the GoAwayRecv's user
    /// handle.
    Received(frame::GoAway),
    /// The user GoAwayRecv has been given the GOAWAY, it awaits in Handled state until the user is
    /// ready for the Connection to proceed.
    Handled(frame::GoAway),
    /// The user is done with the GOAWAY, it is ready for the Connection to take now.
    Pending(frame::GoAway),

    /// The user GoAwayRecv was dropped. If any GOAWAY was pending in Received, Handled, or
    /// Pending, it will be carried here.
    UserGone(Option<frame::GoAway>),
    /// The Connection was dropped. Any GOAWAY that had been Received but not yet given to the user
    /// will be held here.
    ConnGone(Option<frame::GoAway>),
}

// ===== impl GoAwayRecv =====

impl GoAwayRecv {
    pub(crate) fn new() -> (GoAwayRecv, GoAwayRecvConnHandle) {
        let core: Core = Default::default();
        let user = GoAwayRecv(Arc::new(GoAwayRecvInner(core.clone())));
        let conn = GoAwayRecvConnHandle(core);
        (user, conn)
    }

    pub(crate) fn poll_go_away(&mut self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        self.0 .0.poll_go_away(cx)
    }

    pub(crate) fn wake_conn(&self) {
        self.0 .0.wake_conn()
    }
}

// ===== impl GoAwayRecvInner =====

impl Drop for GoAwayRecvInner {
    fn drop(&mut self) {
        self.0.drop_user_handle()
    }
}

// ===== impl GoAwayRecvConnHandle =====

impl GoAwayRecvConnHandle {
    /// A Connection shall use `recv` to share any received GOAWAY frame with the user-owned
    /// GoAwayRecv handle (ie, crate::share::GoAwayRecv). After providing a frame, the Connection
    /// must wait until `poll_conn_ready` returns Ready.
    ///
    /// It should do as little as possible until then, to allow a user to do whatever book keeping
    /// may be necessary. This will `panic!` if a prior frame has not been taken by the user
    /// handle.
    pub(crate) fn recv(&mut self, frame: frame::GoAway) {
        self.0.recv(frame)
    }

    pub(crate) fn poll_conn_ready(&mut self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        self.0.poll_conn_ready(cx)
    }
}

impl Drop for GoAwayRecvConnHandle {
    fn drop(&mut self) {
        self.0.drop_conn_handle()
    }
}

// ===== impl Core =====

macro_rules! maybe_wake {
    ($w:expr) => {
        if let Some(waker) = $w {
            waker.wake();
        }
    };
}

impl Core {
    // For GoAwayRecvConnHandle...

    fn recv(&self, frame: frame::GoAway) {
        maybe_wake!(self.lock().recv(frame));
    }

    fn poll_conn_ready(&self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        self.lock().poll_conn_ready(cx)
    }

    fn drop_conn_handle(&self) {
        maybe_wake!(self.lock().drop_conn_handle());
    }

    // For GoAwayRecv (user)...

    fn poll_go_away(&self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        self.lock().poll_go_away(cx)
    }

    fn wake_conn(&self) {
        maybe_wake!(self.lock().wake_conn());
    }

    fn drop_user_handle(&self) {
        maybe_wake!(self.lock().drop_user_handle());
    }

    // Internal only...

    fn lock(&self) -> MutexGuard<CoreInner> {
        self.0.lock().unwrap()
    }
}

// ===== impl CoreInner =====

// Except for the GoAwayRecv(ConnHandle) drop() handlers, every other CoreInner method implies a
// possible state advancement, returning _something_ based on the prior state.  This macro aims to
// simply make it clear (and ensure) that each is doing something very similar. (And while the drop
// handlers almost do the same sort of thing, they do so in a different form)
macro_rules! next_return {
    ($state:expr, $matches:tt) => {{
        let (next, ret) = match ::std::mem::take(&mut $state) $matches;
        $state = next;
        return ret;
    }}
}

impl CoreInner {
    #[must_use]
    fn recv(&mut self, frame: frame::GoAway) -> Option<Waker> {
        tracing::trace!("CoreInner::recv, {:?}", self.state);
        next_return!(self.state, {
            RecvState::Empty => (RecvState::Received(frame), self.wake_user.take()),
            same @ RecvState::UserGone(None) => (same, None),
            invalid => panic!("CoreInner::recv called with state {:?}", invalid),
        })
    }

    fn poll_go_away(&mut self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        tracing::trace!("CoreInner::poll_go_away, {:?}", self.state);
        next_return!(self.state, {
            same @ (RecvState::Empty | RecvState::Handled(_) | RecvState::Pending(_)) => {
                self.wake_user = Some(cx.waker().clone());
                (same, Poll::Pending)
            }
            RecvState::Received(go_away) => {
                (RecvState::Handled(go_away.clone()), Poll::Ready(Some(go_away)))
            }
            RecvState::ConnGone(maybe_go_away) => {
                (RecvState::ConnGone(None), Poll::Ready(maybe_go_away))
            }
            invalid => panic!("CoreInner::poll_go_away called with state {:?}", invalid),
        })
    }

    fn poll_conn_ready(&mut self, cx: &mut Context) -> Poll<Option<frame::GoAway>> {
        tracing::trace!("CoreInner::poll_conn_ready, {:?}", self.state);
        next_return!(self.state, {
            same @ RecvState::Empty => (same, Poll::Ready(None)),
            same @ (RecvState::Received(_) | RecvState::Handled(_)) => {
                self.wake_conn = Some(cx.waker().clone());
                (same, Poll::Pending)
            }
            RecvState::Pending(go_away) => {
                (RecvState::Empty, Poll::Ready(Some(go_away)))
            }
            RecvState::UserGone(maybe_go_away) => (RecvState::UserGone(None), Poll::Ready(maybe_go_away)),
            invalid => panic!("CoreInner::poll_conn_ready called with state {:?}", invalid),
        })
    }

    #[must_use]
    fn wake_conn(&mut self) -> Option<Waker> {
        tracing::trace!("CoreInner::wake_conn, {:?}", self.state);
        next_return!(self.state, {
            RecvState::Handled(go_away) => {
                (RecvState::Pending(go_away), self.wake_conn.take())
            }
            // If the Connection was dropped while the user was handling a GoAway, there cannot be
            // another waiting.
            same @ RecvState::ConnGone(None) => (same, None),
            // state cannot be UserGone, even if the GoAwayRecv initially given to the user was
            // dropped. wake_conn must only be called by a GoAwayRecv,
            invalid => panic!("CoreInner::wake_conn called with state {:?}", invalid),
        })
    }

    #[must_use]
    fn drop_conn_handle(&mut self) -> Option<Waker> {
        tracing::trace!("CoreInner::drop_conn_handle, {:?}", self.state);
        let prior = std::mem::take(self);
        self.state = RecvState::ConnGone(match prior.state {
            RecvState::Received(go_away) => Some(go_away),
            _ => None,
        });
        prior.wake_user
    }

    #[must_use]
    fn drop_user_handle(&mut self) -> Option<Waker> {
        tracing::trace!("CoreInner::drop_user_handle, {:?}", self.state);
        let prior = std::mem::take(self);
        self.state = RecvState::UserGone(match prior.state {
            RecvState::Received(go_away)
            | RecvState::Handled(go_away)
            | RecvState::Pending(go_away) => Some(go_away),
            _ => None,
        });
        prior.wake_conn
    }
}

impl Default for CoreInner {
    fn default() -> Self {
        CoreInner {
            state: RecvState::Empty,
            wake_conn: None,
            wake_user: None,
        }
    }
}

impl Default for RecvState {
    fn default() -> Self {
        RecvState::Empty
    }
}
