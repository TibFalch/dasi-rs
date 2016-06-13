use std::sync::mpsc;
#[cfg(feature="voice")]
use std::collections::HashMap;

use websocket::client::{Client, Sender, Receiver};
use websocket::stream::WebSocketStream;

use serde_json;
use serde_json::builder::ObjectBuilder;

use model::*;
use internal::Status;
#[cfg(feature="voice")]
use voice::VoiceConnection;
use {Result, Error, SenderExt, ReceiverExt};

/// Websocket connection to the Discord servers.
pub struct Connection {
    keepalive_channel: mpsc::Sender<Status>,
    receiver: Receiver<WebSocketStream>,
    #[cfg(feature="voice")]
    voice_handles: HashMap<ServerId, VoiceConnection>,
    #[cfg(feature="voice")]
    user_id: UserId,
    ws_url: String,
    token: String,
    session_id: Option<String>,
    last_sequence: u64,
}

impl Connection {
    /// Establish a connection to the Discord websocket servers.
    ///
    /// Returns both the `Connection` and the `ReadyEvent` which is always the
    /// first event received and contains initial state information.
    ///
    /// Usually called internally by `Discord::connect`, which provides both
    /// the token and URL.
    pub fn new(base_url: &str, token: &str) -> Result<(Connection, ReadyEvent)> {
        debug!("Gateway: {}", base_url);
        // establish the websocket connection
        let url = match ::websocket::client::request::Url::parse(&format!("{}?v={}", base_url, ::GATEWAY_VERSION)) {
            Ok(url) => url,
            Err(_) => return Err(Error::Other("Invalid URL in Connection::new()"))
        };
        let response = try!(try!(Client::connect(url)).send());
        try!(response.validate());
        let (mut sender, mut receiver) = response.begin().split();

        // send the handshake
        let identify = identify(token);
        try!(sender.send_json(&identify));

        // read the Ready event
        let sequence;
        let ready;
        loop {
            match try!(receiver.recv_json(GatewayEvent::decode)) {
                GatewayEvent::Dispatch(seq, Event::Ready(event)) => {
                    sequence = seq;
                    ready = event;
                    break
                },
                GatewayEvent::InvalidateSession => {
                    debug!("Session invalidated, reidentifying");
                    try!(sender.send_json(&identify))
                }
                other => {
                    debug!("Unexpected event: {:?}", other);
                    return Err(Error::Protocol("Unexpected event during connection open"))
                }
            }
        }
        if ready.version != ::GATEWAY_VERSION {
            warn!("Got protocol version {} instead of {}", ready.version, ::GATEWAY_VERSION);
        }
        let session_id = ready.session_id.clone();
        let heartbeat_interval = ready.heartbeat_interval;

        // spawn the keepalive thread
        let (tx, rx) = mpsc::channel();
        try!(::std::thread::Builder::new()
            .name("Discord Keepalive".into())
            .spawn(move || keepalive(heartbeat_interval, sender, rx)));

        // return the connection
        Connection::inner_new(tx, receiver, base_url.to_owned(), token.to_owned(),
            Some(session_id), sequence, ready)
    }

    #[cfg(not(feature="voice"))]
    fn inner_new(keepalive_channel: mpsc::Sender<Status>,
    receiver: Receiver<WebSocketStream>,
    ws_url: String,
    token: String,
    session_id: Option<String>,
    last_sequence: u64, ready: ReadyEvent) -> Result<(Self, ReadyEvent)> {
        Ok((Connection {
            keepalive_channel: keepalive_channel,
            receiver: receiver,
            ws_url: ws_url,
            token: token,
            session_id: session_id,
            last_sequence: last_sequence,
        }, ready))
    }

    #[cfg(feature="voice")]
    fn inner_new(keepalive_channel: mpsc::Sender<Status>,
    receiver: Receiver<WebSocketStream>,
    ws_url: String,
    token: String,
    session_id: Option<String>,
    last_sequence: u64, ready: ReadyEvent) -> Result<(Self, ReadyEvent)> {
        Ok((Connection {
            keepalive_channel: keepalive_channel,
            receiver: receiver,
            voice_handles: HashMap::new(),
            user_id: ready.user.id,
            ws_url: ws_url,
            token: token,
            session_id: session_id,
            last_sequence: last_sequence,
        }, ready))
    }

    /// Change the game information that this client reports as playing.
    pub fn set_game(&self, game: Option<Game>) {
        let msg = ObjectBuilder::new()
            .insert("op", 3)
            .insert_object("d", move |mut object| {
                object = object.insert("idle_since", serde_json::Value::Null);
                match game {
                    Some(game) => object.insert_object("game", move |o| o.insert("name", game.name)),
                    None => object.insert("game", serde_json::Value::Null),
                }
            })
            .unwrap();
        let _ = self.keepalive_channel.send(Status::SendMessage(msg));
    }

    /// Set the client to be playing this game, with defaults used for any
    /// extended information.
    pub fn set_game_name(&self, name: String) {
        self.set_game(Some(Game::playing(name)));
    }

    /// Get a handle to the voice connection for a server.
    #[cfg(feature="voice")]
    pub fn voice(&mut self, server_id: ServerId) -> &mut VoiceConnection {
        let Connection { ref mut voice_handles, user_id, ref keepalive_channel, .. } = *self;
        voice_handles.entry(server_id).or_insert_with(||
            VoiceConnection::__new(server_id, user_id, keepalive_channel.clone())
        )
    }

    /// Drop the voice connection for a server, forgetting all settings.
    ///
    /// Calling `.voice(server_id).disconnect()` will disconnect from voice but retain the mute
    /// and deaf status, audio source, and audio receiver.
    #[cfg(feature="voice")]
    pub fn drop_voice(&mut self, server_id: ServerId) {
        self.voice_handles.remove(&server_id);
    }

    /// Receive an event over the websocket, blocking until one is available.
    pub fn recv_event(&mut self) -> Result<Event> {
        match self.receiver.recv_json(GatewayEvent::decode) {
            Err(Error::WebSocket(err)) => {
                warn!("Websocket error, reconnecting: {:?}", err);
                // Try resuming if we haven't received an InvalidateSession
                if let Some(session_id) = self.session_id.clone() {
                    match self.resume(session_id) {
                        Ok(event) => return Ok(event),
                        Err(e) => debug!("Failed to resume: {:?}", e),
                    }
                }
                self.reconnect().map(Event::Ready)
            }
            Err(Error::Closed(num, message)) => {
                warn!("Closure, reconnecting: {:?}: {}", num, String::from_utf8_lossy(&message));
                // Try resuming if we haven't received a 1000, a 4006, or an InvalidateSession
                if num != Some(1000) && num != Some(4006) {
                    if let Some(session_id) = self.session_id.clone() {
                        match self.resume(session_id) {
                            Ok(event) => return Ok(event),
                            Err(e) => debug!("Failed to resume: {:?}", e),
                        }
                    }
                }
                self.reconnect().map(Event::Ready)
            }
            Err(error) => Err(error),
            Ok(GatewayEvent::Dispatch(sequence, event)) => {
                self.last_sequence = sequence;
                let _ = self.keepalive_channel.send(Status::Sequence(sequence));
                if let Event::Resumed { heartbeat_interval, .. } = event {
                    debug!("Resumed successfully");
                    let _ = self.keepalive_channel.send(Status::ChangeInterval(heartbeat_interval));
                }
                /*TODO fix voice
                if let Event::VoiceStateUpdate(server_id, ref voice_state) = event {
                    self.voice(server_id).__update_state(voice_state);
                }
                if let Event::VoiceServerUpdate { server_id, ref endpoint, ref token } = event {
                   self.voice(server_id).__update_server(endpoint, token);
                }
                */
                Ok(event)
            }
            Ok(GatewayEvent::Heartbeat(sequence)) => {
                debug!("Heartbeat received with seq {}", sequence);
                let map = ObjectBuilder::new()
                    .insert("op", 1)
                    .insert("d", sequence)
                    .unwrap();
                let _ = self.keepalive_channel.send(Status::SendMessage(map));
                self.recv_event()
            }
            Ok(GatewayEvent::Reconnect) => {
                self.reconnect().map(Event::Ready)
            }
            Ok(GatewayEvent::InvalidateSession) => {
                debug!("Session invalidated, reidentifying");
                self.session_id = None;
                let _ = self.keepalive_channel.send(Status::SendMessage(identify(&self.token)));
                self.recv_event()
            }
        }
    }

    /// Reconnect after receiving an OP7 RECONNECT
    fn reconnect(&mut self) -> Result<ReadyEvent> {
        debug!("Reconnecting...");
        // Make two attempts on the current known gateway URL
        for _ in 0..2 {
            if let Ok((conn, ready)) = Connection::new(&self.ws_url, &self.token) {
                try!(::std::mem::replace(self, conn).shutdown());
                self.session_id = Some(ready.session_id.clone());
                return Ok(ready)
            }
            ::sleep_ms(1000);
        }
        // If those fail, hit REST for a new endpoint
        let (conn, ready) = try!(::Discord {
            client: ::hyper::client::Client::new(),
            token: self.token.to_owned()
        }.connect());
        try!(::std::mem::replace(self, conn).shutdown());
        self.session_id = Some(ready.session_id.clone());
        Ok(ready)
    }

    /// Resume using our existing session
    fn resume(&mut self, session_id: String) -> Result<Event> {
        debug!("Resuming...");
        // close connection and re-establish
        try!(self.receiver.get_mut().get_mut().shutdown(::std::net::Shutdown::Both));
        let url = match ::websocket::client::request::Url::parse(&format!("{}?v={}", self.ws_url, ::GATEWAY_VERSION)) {
            Ok(url) => url,
            Err(_) => return Err(Error::Other("Invalid URL in Connection::resume()"))
        };
        let response = try!(try!(Client::connect(url)).send());
        try!(response.validate());
        let (mut sender, mut receiver) = response.begin().split();

        // send the resume request
        let resume = ObjectBuilder::new()
            .insert("op", 6)
            .insert_object("d", |o| o
                .insert("seq", self.last_sequence)
                .insert("token", &self.token)
                .insert("session_id", session_id)
            )
            .unwrap();
        try!(sender.send_json(&resume));

        // TODO: when Discord has implemented it, observe the RESUMING event here
        let first_event;
        loop {
            match try!(receiver.recv_json(GatewayEvent::decode)) {
                GatewayEvent::Dispatch(seq, event) => {
                    if let Event::Ready(ReadyEvent { ref session_id, .. }) = event {
                        self.session_id = Some(session_id.clone());
                    }
                    self.last_sequence = seq;
                    first_event = event;
                    break
                },
                GatewayEvent::InvalidateSession => {
                    debug!("Session invalidated in resume, reidentifying");
                    try!(sender.send_json(&identify(&self.token)));
                }
                other => {
                    debug!("Unexpected event: {:?}", other);
                    return Err(Error::Protocol("Unexpected event during resume"))
                }
            }
        }

        // switch everything to the new connection
        self.receiver = receiver;
        let _ = self.keepalive_channel.send(Status::ChangeSender(sender));
        Ok(first_event)
    }

    /// Cleanly shut down the websocket connection. Optional.
    pub fn shutdown(mut self) -> Result<()> {
        try!(self.receiver.get_mut().get_mut().shutdown(::std::net::Shutdown::Both));
        Ok(())
    }

    #[doc(hidden)]
    pub fn __download_members(&self, servers: &[ServerId]) {
        let msg = ObjectBuilder::new()
            .insert("op", 8)
            .insert_object("d", |o| o
                .insert_array("guild_id", |a| servers.iter().fold(a, |a, s| a.push(s.0)))
                .insert("query", "")
                .insert("limit", 0)
            )
            .unwrap();
        let _ = self.keepalive_channel.send(Status::SendMessage(msg));
    }
}

fn identify(token: &str) -> serde_json::Value {
    ObjectBuilder::new()
        .insert("op", 2)
        .insert_object("d", |object| object
            .insert("token", token)
            .insert_object("properties", |object| object
                .insert("$os", ::std::env::consts::OS)
                .insert("$browser", "Discord library for Rust")
                .insert("$device", "discord-rs")
                .insert("$referring_domain", "")
                .insert("$referrer", "")
            )
            .insert("v", ::GATEWAY_VERSION)
        )
        .unwrap()
}

fn keepalive(interval: u64, mut sender: Sender<WebSocketStream>, channel: mpsc::Receiver<Status>) {
    let mut timer = ::Timer::new(interval);
    let mut last_sequence = 0;

    'outer: loop {
        ::sleep_ms(100);

        loop {
            match channel.try_recv() {
                Ok(Status::SendMessage(val)) => {
                    match sender.send_json(&val) {
                        Ok(()) => {},
                        Err(e) => warn!("Error sending gateway message: {:?}", e)
                    }
                },
                Ok(Status::Sequence(seq)) => {
                    last_sequence = seq;
                },
                Ok(Status::ChangeInterval(interval)) => {
                    timer = ::Timer::new(interval);
                },
                Ok(Status::ChangeSender(new_sender)) => {
                    sender = new_sender;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'outer,
            }
        }

        if timer.check_tick() {
            let map = ObjectBuilder::new()
                .insert("op", 1)
                .insert("d", last_sequence)
                .unwrap();
            match sender.send_json(&map) {
                Ok(()) => {},
                Err(e) => warn!("Error sending gateway keeaplive: {:?}", e)
            }
        }
    }
    let _ = sender.get_mut().shutdown(::std::net::Shutdown::Both);
}
