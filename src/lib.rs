extern crate rustc_serialize;
extern crate websocket;

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::thread::JoinHandle;
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

mod requests;
mod responses;

use responses::*;
use requests::*;

type Client = websocket::Client<DataFrame, sender::Sender<WebSocketStream>, receiver::Receiver<WebSocketStream>>;

pub struct DdpClient {
    receiver: JoinHandle<()>,
    sender:   JoinHandle<()>,
    session_id: Arc<String>,
    version: &'static str,
}

impl DdpClient {
    pub fn new(url: &Url) -> Result<Self, DdpConnError> {
        let (client, session_id, v_index) = try!( DdpClient::connect(url) );
        let (mut sender, mut receiver) = client.split();

        let (tx, rx) = channel();

        let receiver_loop = thread::spawn(move || {
            for message in receiver.incoming_messages() {
                let message = match message {
                    Ok(Message::Text(m))  => m,
                    // TODO: Something more meaningful should happen.
                    _ => continue,
                };

                if let Some(ping) = Ping::from_response(&message) {
                    tx.send(Pong {
                        msg: "pong",
                        id:  ping.id,
                    }).unwrap();
                }
            }
        });

        let sender_loop = thread::spawn(move || {
            while let Ok(message) = rx.recv() {
                let message = Message::Text(json::encode(&message).unwrap());
                sender.send_message(message).unwrap();
            }
        });

        Ok(DdpClient {
            receiver:   receiver_loop,
            sender:     sender_loop,
            session_id: Arc::new(session_id),
            version:    VERSIONS[v_index],
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

    fn negotiate(client: &mut Client, version: &str) -> Result<NegotiateResp, DdpConnError> {
        let request = requests::Connect::new(version);
        let request = Message::Text(request.to_json());

        try!( client.send_message(request).map_err(|e| DdpConnError::Network(e)) );

        for msg_result in client.incoming_messages() {
            if let Ok(Message::Text(plaintext)) = msg_result {
                if let Some(success) = VersionSuccess::from_response(&plaintext) {
                    return Ok(NegotiateResp::SessionId(success.session));
                } else if let Some(failure) = VersionFailed::from_response(&plaintext) {
                    return Ok(NegotiateResp::Version(failure.version));
                }
            }
        }
        // TODO: This is probably unreachable
        Err(DdpConnError::NoVersionFromServer)
    }

    fn connect(url: &Url) -> Result<(Client, String, usize), DdpConnError> {
        let mut client = try!( DdpClient::handshake(url) );
        let mut version = VER_NOW;
        let mut v_index = 0;

        loop {
            match DdpClient::negotiate(&mut client, version) {
                Err(e) => return Err(e),
                Ok(NegotiateResp::SessionId(session)) => return Ok((client, session, v_index)),
                Ok(NegotiateResp::Version(server_version)) => {
                    // TODO: Maybe this should be faster, maybe its enough.
                    let found = VERSIONS.iter().enumerate().find(|&(_, &v)| *v == server_version);
                    if let Some((i, &v)) = found {
                        v_index = i;
                        version = v;
                    } else {
                        return Err(DdpConnError::NoMatchingVersion);
                    }
                },
            };
        }
    }

    pub fn block_until_err(self) {
        self.receiver.join().ok();
        self.sender.join().ok();
    }

    pub fn session(&self) -> &str {
        &self.session_id
    }

    pub fn version(&self) -> &str {
        &self.version
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

    client.block_until_err();
}
