use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use futures_util::{Stream, future::poll_fn};
use parking_lot::Mutex;

use crate::types::{AssistantMessage, AssistantMessageEvent};

#[derive(Default)]
struct State {
    queue: VecDeque<AssistantMessageEvent>,
    done: bool,
    final_result: Option<AssistantMessage>,
    stream_waker: Option<Waker>,
    result_wakers: Vec<Waker>,
}

/// Push-driven stream matching pi's `AssistantMessageEventStream` contract.
///
/// Clones share the same queue and are intended for producer handles. As with a
/// Tokio receiver, multiple consumers divide events rather than broadcasting.
#[derive(Clone, Default)]
pub struct AssistantMessageEventStream {
    state: Arc<Mutex<State>>,
}

impl AssistantMessageEventStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, event: AssistantMessageEvent) {
        let mut state = self.state.lock();
        if state.done {
            return;
        }

        if let Some(message) = event.final_message() {
            state.done = true;
            state.final_result = Some(message.clone());
            for waker in state.result_wakers.drain(..) {
                waker.wake();
            }
        }

        state.queue.push_back(event);
        if let Some(waker) = state.stream_waker.take() {
            waker.wake();
        }
    }

    /// Ends iteration. Supplying a result also resolves [`Self::result`].
    pub fn end(&self, result: Option<AssistantMessage>) {
        let mut state = self.state.lock();
        state.done = true;
        if result.is_some() {
            state.final_result = result;
            for waker in state.result_wakers.drain(..) {
                waker.wake();
            }
        }
        if let Some(waker) = state.stream_waker.take() {
            waker.wake();
        }
    }

    pub fn is_complete(&self) -> bool {
        self.state.lock().done
    }

    pub async fn result(&self) -> AssistantMessage {
        poll_fn(|cx| {
            let mut state = self.state.lock();
            if let Some(result) = &state.final_result {
                return Poll::Ready(result.clone());
            }
            if !state.result_wakers.iter().any(|w| w.will_wake(cx.waker())) {
                state.result_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock();
        if let Some(event) = state.queue.pop_front() {
            return Poll::Ready(Some(event));
        }
        if state.done {
            return Poll::Ready(None);
        }
        state.stream_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    AssistantMessageEventStream::new()
}
