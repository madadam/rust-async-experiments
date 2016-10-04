extern crate futures;
extern crate maidsafe_utilities;
extern crate rand;
#[macro_use]
extern crate unwrap;

use futures::{Async, Future, Poll};
use maidsafe_utilities::thread::Joiner;
use self::routing::Client as RoutingClient;
use self::routing::Event as RoutingEvent;
use self::routing::{Data, Error, MessageId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

fn main() {
    let el = EventLoop::new();

    el.post(|client| {
        client.borrow_mut().get(true).map(|_| {
            println!("get 0 success");
        }).map_err(|_| {
            println!("get 0 failure");
        })
    });

    el.post(|client| {
        client.borrow_mut().get(false).map(|_| {
            println!("get 1 success");
        }).map_err(|_| {
            println!("get 1 failure");
        })
    });

    el.post(|client| {
        let client2 = client.clone();

        let mut client = client.borrow_mut();
        client.get(true).and_then(move |_| {
            println!("get 2 success");
            let mut client = client2.borrow_mut();
            client.get(false)
        }).map(|_| {
            println!("get 3 success");
        }).map_err(|_| {
            println!("get 2 or 3 failure");
        })
    });

    thread::sleep(Duration::from_millis(1000));
}

type LeafFuture = Future<Item=(), Error=()>;

struct EventLoop {
    event_tx: Sender<Event>,
    _routing_joiner: Joiner,
    _main_joiner: Joiner,
}

impl EventLoop {
    fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel();

        let (routing, routing_joiner) = {
            let event_tx = event_tx.clone();

            let (tx, rx) = mpsc::channel();
            let client = RoutingClient::new(tx);
            // Forward routing event to main events.
            let joiner = Joiner::new(thread::spawn(move || {
                for event in rx {
                    let _ = event_tx.send(Event::Routing(event));
                }
            }));

            (client, joiner)
        };

        let main_joiner = Joiner::new(thread::spawn(move || {
            let client = Rc::new(RefCell::new(Client::new(routing)));
            let mut pending_futures = Vec::new();

            for event in event_rx {
                match event {
                    Event::User(mut action) => {
                        let mut future = action.call(client.clone());
                        if let Ok(Async::NotReady) = future.poll() {
                            pending_futures.push(future);
                        }
                    },
                    Event::Routing(event) => {
                        client.borrow_mut().handle_routing_event(event);
                        Self::poll_all(&mut pending_futures);
                    },
                    Event::Terminate => break,
                }
            }
        }));

        EventLoop {
            event_tx: event_tx,
            _routing_joiner: routing_joiner,
            _main_joiner: main_joiner,
        }
    }

    // Post a closure to the event loop thread. The closure should evaluate to
    // a future, which will get driven to completion on the event loop thread.
    fn post<F, R>(&self, f: F)
        where F: FnOnce(Rc<RefCell<Client>>) -> R + Send + 'static,
              R: Future + 'static
    {
        let f = move |client| -> Box<LeafFuture> {
            Box::new(f(client).map(|_| ()).map_err(|_| ()))
        };
        let _ = self.event_tx.send(Event::User(UserAction::new(f)));
    }

    fn poll_all(futures: &mut Vec<Box<LeafFuture>>) {
        // TODO: remove completed futures.
        for future in futures.iter_mut() {
            let _ = future.poll();
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        let _ = self.event_tx.send(Event::Terminate);
    }
}

struct UserAction(Box<FnMut(Rc<RefCell<Client>>) -> Box<LeafFuture> + Send>);

impl UserAction {
    fn new<F>(f: F) -> Self
        where F: FnOnce(Rc<RefCell<Client>>) -> Box<LeafFuture> + Send + 'static
    {
        let mut f = Some(f);

        UserAction(Box::new(move |client| {
            let f = unwrap!(f.take());
            f(client)
        }))
    }

    fn call(&mut self, client: Rc<RefCell<Client>>) -> Box<LeafFuture> {
        (*self.0)(client)
    }
}

enum Event {
    User(UserAction),
    Routing(RoutingEvent),
    Terminate,
}

struct Client {
    routing: RoutingClient,
    pending_requests: HashMap<MessageId, ResponseHolder>,
}

impl Client {
    fn new(routing: RoutingClient) -> Self {
        Client {
            routing: routing,
            pending_requests: HashMap::new(),
        }
    }

    fn get(&mut self, will_succeed: bool) -> Box<Future<Item=Data, Error=Error>> {
        let holder = Rc::new(RefCell::new(None));
        let message_id = MessageId::new();

        let _ = self.pending_requests.insert(message_id, holder.clone());
        self.routing.send_get_request(message_id, will_succeed);

        Box::new(Response(holder))
    }

    fn handle_routing_event(&mut self, event: RoutingEvent) {
        let (key, result) = match event {
            RoutingEvent::GetSuccess(id, data) => (id, Ok(data)),
            RoutingEvent::GetFailure(id, error) => (id, Err(error)),
        };

        if let Some(holder) = self.pending_requests.remove(&key) {
            *holder.borrow_mut() = Some(result);
        }
    }
}

type ResponseHolder = Rc<RefCell<Option<Result<Data, Error>>>>;

// Pending response from routing.
struct Response(ResponseHolder);

impl Future for Response {
    type Item = Data;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.borrow_mut().take() {
            Some(Ok(data)) => Ok(Async::Ready(data)),
            Some(Err(error)) => Err(error),
            None => Ok(Async::NotReady),
        }
    }
}

// Mock routing
mod routing {
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::Duration;
    use rand;

    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct MessageId(u64);

    impl MessageId {
        pub fn new() -> Self {
            MessageId(rand::random())
        }
    }

    pub struct Data;
    pub struct Error;

    pub enum Event {
        GetSuccess(MessageId, Data),
        GetFailure(MessageId, Error),
    }

    pub struct Client {
        event_tx: Sender<Event>,
    }

    impl Client {
        pub fn new(event_tx: Sender<Event>) -> Self {
            Client {
                event_tx: event_tx,
            }
        }

        pub fn send_get_request(&self, id: MessageId, will_succeed: bool) {
            let tx = self.event_tx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                if will_succeed {
                    tx.send(Event::GetSuccess(id, Data))
                } else {
                    tx.send(Event::GetFailure(id, Error))
                }
            });
        }
    }
}
