extern crate futures;
extern crate maidsafe_utilities;
extern crate rand;
#[macro_use]
extern crate unwrap;

use futures::{Async, Future, Poll};
use futures::task::{self, Spawn, Task, Unpark};
use maidsafe_utilities::thread::Joiner;
use self::routing::Client as RoutingClient;
use self::routing::Event as RoutingEvent;
use self::routing::{Data, Error, MessageId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

fn main() {
    let session = Session::new();

    fn callback(result_code: i32) {
        if result_code == 0 {
            println!("success");
        } else {
            println!("failure: {}", result_code);
        }
    }

    example_ffi_function1(&session, callback);
    example_ffi_function1(&session, callback);
    example_ffi_function2(&session, callback);

    thread::sleep(Duration::from_millis(1000));
}

fn example_ffi_function1(session: &Session, cb: fn(i32)) {
    session.event_loop.post(move |client| {
        client.borrow_mut().get(true).map(move |_| {
            cb(0);
        }).map_err(move |_| {
            cb(-1);
        })
    });
}

fn example_ffi_function2(session: &Session, cb: fn(i32)) {
    session.event_loop.post(move |client| {
        let client2 = client.clone();

        let mut client = client.borrow_mut();
        client.get(true).and_then(move |_| {
            let mut client = client2.borrow_mut();
            client.get(false)
        }).map(move |_| {
            cb(0);
        }).map_err(move |_| {
            cb(-1);
        })
    });
}

struct Session {
    event_loop: EventLoop,
}

impl Session {
    pub fn new() -> Self {
        Session {
            event_loop: EventLoop::new(),
        }
    }
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
            let mut driver = Driver::new();

            for event in event_rx {
                match event {
                    Event::User(mut action) => {
                        let future = action.call(client.clone());
                        driver.drive(future);
                    },
                    Event::Routing(event) => {
                        client.borrow_mut().handle_routing_event(event);
                        driver.notify();
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
        // Convert the future returned from the closure to
        // Box<Future<Item=(), Error=()>>
        let f = move |client| -> Box<LeafFuture> {
            Box::new(f(client).map(|_| ()).map_err(|_| ()))
        };
        let _ = self.event_tx.send(Event::User(UserAction::new(f)));
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
    pending_requests: HashMap<MessageId, Rc<RefCell<ResponseData>>>,
}

impl Client {
    fn new(routing: RoutingClient) -> Self {
        Client {
            routing: routing,
            pending_requests: HashMap::new(),
        }
    }

    fn get(&mut self, will_succeed: bool) -> Response {
        let response_data = Rc::new(RefCell::new(ResponseData::new()));
        let message_id = MessageId::new();

        let _ = self.pending_requests.insert(message_id, response_data.clone());
        self.routing.send_get_request(message_id, will_succeed);

        Response(response_data)
    }

    fn handle_routing_event(&mut self, event: RoutingEvent) {
        let (id, result) = match event {
            RoutingEvent::GetSuccess(id, data) => (id, Ok(data)),
            RoutingEvent::GetFailure(id, error) => (id, Err(error)),
        };

        let task = if let Some(data) = self.pending_requests.remove(&id) {
            let mut data = data.borrow_mut();
            data.result = Some(result);
            data.task.take()
        } else {
            None
        };

        if let Some(task) = task {
            task.unpark()
        }
    }
}

struct Response(Rc<RefCell<ResponseData>>);

struct ResponseData {
    result: Option<Result<Data, Error>>,
    task: Option<Task>,
}

impl ResponseData {
    fn new() -> Self {
        ResponseData {
            result: None,
            task: None,
        }
    }
}

impl Future for Response {
    type Item = Data;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut data = self.0.borrow_mut();
        match data.result.take() {
            Some(Ok(data)) => Ok(Async::Ready(data)),
            Some(Err(error)) => Err(error),
            None => {
                data.task = Some(task::park());
                Ok(Async::NotReady)
            }
        }
    }
}

// Helper type to drive futures to completion.
struct Driver {
    next_id: usize,
    spawns: HashMap<usize, Spawn<Box<LeafFuture>>>,
    completed_id: Arc<AtomicUsize>,
}

impl Driver {
    pub fn new() -> Self {
        Driver {
            next_id: 1,
            spawns: HashMap::new(),
            completed_id: Arc::new(AtomicUsize::new(0)),
        }
    }

    // Attempt to drive the future to completion. This either happens now, or
    // at some later time during the call to `notify`.
    pub fn drive(&mut self, future: Box<LeafFuture>) {
        let id = self.next_id();
        self.poll(id, task::spawn(future));
    }

    // Notify the driver that some futures became completed.
    pub fn notify(&mut self) {
        let id = self.completed_id.swap(0, Ordering::Relaxed);
        if id == 0 { return; }

        if let Some(spawn) = self.spawns.remove(&id) {
            self.poll(id, spawn);
        }
    }

    fn next_id(&mut self) -> usize {
        let result = self.next_id;
        self.next_id += 1;
        result
    }

    fn poll(&mut self, id: usize, mut spawn: Spawn<Box<LeafFuture>>) {
        let guard = Notifier(id, self.completed_id.clone());
        if let Ok(Async::NotReady) = spawn.poll_future(Arc::new(guard)) {
            let _ = self.spawns.insert(id, spawn);
        }
    }
}

struct Notifier(usize, Arc<AtomicUsize>);

impl Unpark for Notifier {
    fn unpark(&self) {
        self.1.store(self.0, Ordering::Relaxed)
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
