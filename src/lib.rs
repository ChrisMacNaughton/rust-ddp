#[macro_use]
extern crate log;
extern crate rustc_serialize;
extern crate websocket;

use std::collections::hash_map::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender as AtomicSender;
use std::thread;
use std::thread::JoinHandle;
// use rand::Rng;
use rustc_serialize::json;
use websocket::Message;
use websocket::client::request::Url;
use websocket::dataframe::DataFrame;
use websocket::client::sender;
use websocket::client::receiver;
use websocket::ws::receiver::Receiver;
use websocket::ws::sender::Sender;
use websocket::stream::WebSocketStream;
use websocket::result::WebSocketError;

mod messages;
use messages::*;

mod random;
use random::Random;

mod collections;
use collections::MongoCallbacks;
use collections::Subscriptions;

pub use collections::MongoCollection;
pub use collections::ListenerId;

type Client = websocket::Client<DataFrame, sender::Sender<WebSocketStream>, receiver::Receiver<WebSocketStream>>;
type MethodCallback = Box<FnMut(Result<&Ejson, &Ejson>) + Send + 'static>;

pub struct Methods {
    outgoing:        Arc<Mutex<AtomicSender<String>>>,
    pending_methods: HashMap<String, MethodCallback>,
    rng: Random,
}

impl Methods {
    fn new(outgoing: Arc<Mutex<AtomicSender<String>>>) -> Self {
        Methods {
            rng:             Random::new(),
            pending_methods: HashMap::new(),
            outgoing:        outgoing,
        }
    }

    fn send<C>(&mut self, method: &str, params: Option<&Vec<&Ejson>>, callback: C)
    where C: FnMut(Result<&Ejson, &Ejson>) + Send + 'static {
        let id = self.rng.id();
        let method = Method::text(&id, method, params);
        DdpClient::send(method, &self.outgoing);
        self.pending_methods.insert(id, Box::new(callback));
    }

    fn apply(&mut self, id: &str, response: Result<&Ejson, &Ejson>) {
        if let Some(method) = self.pending_methods.remove(id) {
            let mut method: MethodCallback = method;
            method(response);
        }
    }
}

pub struct DdpClient {
    methods:    Arc<Mutex<Methods>>,
    mongos:     Arc<Mutex<HashMap<String, Arc<Mutex<MongoCallbacks>>>>>,
    subs:       Arc<Mutex<Subscriptions>>,
    session_id: String,
    receiver:   JoinHandle<()>,
    sender:     JoinHandle<()>,
    version:    &'static str,
}

impl DdpClient {
    pub fn new(url: &Url) -> Result<Self, DdpConnError> {
        let (client, session_id, v_index) = try!( DdpClient::connect(url) );
        let (mut sender, mut receiver) = client.split();

        let (tx, rx) = channel();
        let tx = Arc::new(Mutex::new(tx));
        let tx_looper = tx.clone();

        let methods = Arc::new(Mutex::new(Methods::new(tx.clone())));
        let methods_looper = methods.clone();

        let mongos = Arc::new(Mutex::new(HashMap::new()));
        let message_mongos = mongos.clone();

        let subs = Arc::new(Mutex::new(Subscriptions::new(tx.clone())));
        let message_subs = subs.clone();

        let receiver_loop = thread::spawn(move || {
            for packet in receiver.incoming_messages() {
                let message_text = match packet {
                    Ok(Message::Text(m))  => m,
                    // TODO: Something more meaningful should happen.
                    _ => continue,
                };

                println!("<- {}", &message_text);

                let message_json = json::Json::from_str(&message_text).unwrap();
                let message = match message_json.as_object() {
                    Some(o) => o,
                    _ => continue,
                };

                // TODO: Stop assuming received messages are in spec
                match message.get("msg").unwrap().as_string() {
                    Some("ping") => {
                        let id = message.get("id").map(|id| id.as_string().unwrap());
                        DdpClient::send(Pong::text(id), &tx_looper);
                    },
                    Some("result") => {
                        let id = message.get("id").unwrap().as_string().unwrap();
                        let response = match (message.get("error"), message.get("result")) {
                            (Some(e), None)    => Err(e),
                            (None,    Some(r)) => Ok(r),
                            _                  => continue,
                        };
                        methods_looper.lock().unwrap().apply(id, response);
                    },
                    Some("added") => {
                        let collection = message.get("collection").unwrap().as_string().unwrap();
                        if let Some(mongo) = message_mongos.lock().unwrap().get(collection) {
                            let id = message.get("id").unwrap().as_string().unwrap();
                            let fields = message.get("fields");
                            let mongo: &Arc<Mutex<MongoCallbacks>> = mongo;
                            mongo.lock().unwrap().notify_insert(id, fields);
                        }
                    },
                    Some("changed") => {
                        let collection = message.get("collection").unwrap().as_string().unwrap();
                        if let Some(mongo) = message_mongos.lock().unwrap().get(collection) {
                            let id = message.get("id").unwrap().as_string().unwrap();
                            let fields = message.get("fields");
                            let cleared = message.get("cleared");
                            let mongo: &Arc<Mutex<MongoCallbacks>> = mongo;
                            mongo.lock().unwrap().notify_change(id, fields, cleared);
                        }
                    },
                    Some("removed") => {
                        let collection = message.get("collection").unwrap().as_string().unwrap();
                        if let Some(mongo) = message_mongos.lock().unwrap().get(collection) {
                            let id = message.get("id").unwrap().as_string().unwrap();
                            let mongo: &Arc<Mutex<MongoCallbacks>> = mongo;
                            mongo.lock().unwrap().notify_remove(id);
                        }
                    },
                    Some("ready") => {
                        let ids = message.get("subs").unwrap().as_array().unwrap();
                        let ids: Vec<&str> = ids.iter().map(|id| id.as_string().unwrap()).collect();
                        message_subs.lock().unwrap().notify(Ok(ids));
                    },
                    Some("nosub") => {
                        let id = message.get("id").unwrap().as_string().unwrap();
                        let error = message.get("error").unwrap();
                        message_subs.lock().unwrap().notify(Err((id, error)));
                    },
                    _ => continue,
                }
            }
        });

        let sender_loop = thread::spawn(move || {
            while let Ok(message) = rx.recv() {
                println!("-> {}", &message);
                sender.send_message(Message::Text(message)).unwrap();
            }
        });

        Ok(DdpClient {
            methods:    methods,
            mongos:     mongos,
            subs:       subs,
            session_id: session_id,
            version:    VERSIONS[v_index],
            receiver:   receiver_loop,
            sender:     sender_loop,
        })
    }

    fn handshake(url: &Url) -> Result<Client, DdpConnError> {
        // Handshake with the server
        let knock  = try!( Client::connect(url).map_err(|e| DdpConnError::Network(e)) );
        let answer = try!( knock.send()        .map_err(|e| DdpConnError::Network(e)) );
        try!( answer.validate()                .map_err(|e| DdpConnError::Network(e)) );

        // Get referennce to the client
        Ok(answer.begin())
    }

    fn negotiate(client: &mut Client, version: &'static str) -> Result<NegotiateResp, DdpConnError> {
        let request = Connect::new(version);
        let request = Message::Text(json::encode(&request).unwrap());

        try!( client.send_message(request).map_err(|e| DdpConnError::Network(e)) );

        for msg_result in client.incoming_messages() {
            if let Ok(Message::Text(plaintext)) = msg_result {
                if let Some(success) = Connected::from_response(&plaintext) {
                    return Ok(NegotiateResp::SessionId(success.session));
                } else if let Some(failure) = Failed::from_response(&plaintext) {
                    return Ok(NegotiateResp::Version(failure.version));
                }
            }
        }
        // TODO: This is probably unreachable
        Err(DdpConnError::NoVersionFromServer)
    }

    fn connect(url: &Url) -> Result<(Client, String, usize), DdpConnError> {
        let mut client = try!( DdpClient::handshake(url) );
        let mut v_index = 0;

        loop {
            match DdpClient::negotiate(&mut client, VERSIONS[v_index]) {
                Err(e) => return Err(e),
                Ok(NegotiateResp::SessionId(session)) => return Ok((client, session, v_index)),
                Ok(NegotiateResp::Version(server_version)) => {
                    // TODO: Maybe this should be faster, maybe its enough.
                    let found = VERSIONS.iter().enumerate().find(|&(_, &v)| *v == server_version);
                    v_index = match found {
                        Some((i, _)) => i,
                        _ => return Err(DdpConnError::NoMatchingVersion),
                    };
                },
            };
        }
    }

    pub fn call<C>(&self, method: &str, params: Option<&Vec<&Ejson>>, callback: C)
    where C: FnMut(Result<&Ejson, &Ejson>) + Send + 'static {
        self.methods.lock().unwrap().send(method, params, callback);
    }

    pub fn mongo(&self, collection: &str) -> MongoCollection {
        // TODO: inefficient .get twice, .to_string bad also
        let mut mongos = self.mongos.lock().unwrap();
        let methods  = self.methods.clone();
        let callbacks = mongos.get(collection).map(|o| o.clone()).unwrap_or_else(|| {
            let name = collection.to_string();
            let handler = Arc::new(Mutex::new(MongoCallbacks::new(self.subs.clone(), name)));
            mongos.insert(collection.to_string(), handler);
            mongos.get(collection).unwrap().clone()
        });

        MongoCollection::new(collection.to_string(), methods, callbacks)
    }

    pub fn block_until_err(self) {
        self.receiver.join().ok();
        self.sender.join().ok();
    }

    pub fn session(&self) -> &str {
        &self.session_id
    }

    pub fn version(&self) -> &'static str {
        &self.version
    }

    fn send(message: String, tx: &Arc<Mutex<AtomicSender<String>>>) {
        tx.lock().unwrap().send(message).unwrap();
    }
}

pub enum NegotiateResp {
    SessionId(String),
    Version(String),
}

#[derive(Debug)]
pub enum DdpConnError {
    Network(WebSocketError),
    NoVersionFromServer,
    NoMatchingVersion,
}

#[test]
fn test_connect_version() {
    let url = Url::parse("ws://127.0.0.1:3000/websocket").unwrap(); // Get the URL
    let ddp_client_result = DdpClient::new(&url);

    let client = match ddp_client_result {
        Ok(client) => client,
        Err(err)   => panic!("An error occured: {:?}", err),
    };

    println!("The session id is: {} with DDP v{}", client.session(), client.version());

    println!("\n\nCalling a real method!\n\n");
    client.call("hello", None, |result| {
        print!("Ran method, ");
        match result {
            Ok(output) => println!("got a result: {}", output),
            Err(error) => println!("got an error: {}", error),
        }
    });

    println!("\n\nCalling a fake method!\n\n");
    client.call("not_a_method", None, |result| {
        print!("Ran method, ");
        match result {
            Ok(output) => println!("got a result: {}", output),
            Err(error) => println!("got an error: {}", error),
        }
    });

    println!("\n\nSubscribing to MongoColl!\n\n");
    let mongo = client.mongo("MongoColl");
    mongo.on_add(|id, _| {
        println!("Added record with id: {}", &id);
    });

    println!("\n\nInserting a record!\n\n");
    let record = json::Json::from_str("{ \"first ever meteor data from rust\": true }").unwrap();
    mongo.insert(&record, |result| {
        match result {
            Ok(_) => println!("First every successful insertion into Meteor through rust!"),
            Err(_) =>  println!("Damn! Got an error."),
        };
    });

    println!("\n\nRemoving records...\n\n");
    mongo.remove(&record, |result| {
        println!("removed records {:?}", result);
    });

    println!("Subscribing to non existent collection.");
    let nomongo = client.mongo("SomethingElse");
    nomongo.on_ready(|result| {
        match result {
            Ok(()) => unreachable!(),
            Err(e) => println!("Got an error, this is expected: {}", e),
        }
    });

    client.block_until_err();
}
