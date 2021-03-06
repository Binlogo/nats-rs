use crate::errors::{ErrorKind::*, *};
use crate::stream::{self, Stream};
use rand::{distributions::Alphanumeric, seq::SliceRandom, thread_rng};
use serde::{Deserialize, Serialize};
use serde_json::{de, Value};
use std::{
  collections::HashMap,
  error::Error,
  io::{self, BufRead, BufReader, Write},
  net::TcpStream,
  thread,
  time::{Duration, Instant},
};
use url::Url;

const URI_SCHEME: &str = "nats";
const DEFAULT_PORT: u16 = 4222;
const RETRIES_MAX: u32 = 5;

const CIRCUIT_BREAKER_WAIT_AFTER_BREAKING_MS: u64 = 2000;
const CIRCUIT_BREAKER_WAIT_BETWEEN_ROUNDS_MS: u64 = 250;
const CIRCUIT_BREAKER_ROUNDS_BEFORE_BREAKING: u32 = 4;

#[derive(Debug, Copy, Clone)]
pub struct Channel {
  pub sid: u64,
}

#[derive(Debug)]
pub struct Event {
  pub subject: String,
  pub channel: Channel,
  pub msg: Vec<u8>,
  pub inbox: Option<String>,
}

#[derive(Debug)]
pub struct Events<'a> {
  client: &'a mut Client,
}

#[derive(Debug)]
pub struct Client {
  servers_info: Vec<ServerInfo>,
  server_idx: usize,
  verbose: bool,
  state: Option<ClientState>,
  sid: u64,
  subscriptions: HashMap<u64, Subscription>,
}

impl Client {
  pub fn new<T: ToStringVec>(uris: T) -> Result<Client, NatsClientError> {
    let mut servers_info = Vec::new();
    for uri in uris.to_string_vec() {
      let parsed = parse_nats_uri(&uri)?;
      let host = parsed
        .host_str()
        .ok_or((InvalidClientConfig, "Missing host"))?
        .to_owned();
      let port = parsed.port().unwrap_or(DEFAULT_PORT);
      servers_info.push(ServerInfo { host, port });
    }
    let mut rng = thread_rng();
    servers_info.shuffle(&mut rng);
    Ok(Client {
      servers_info,
      server_idx: 0,
      verbose: true,
      state: None,
      sid: 1,
      subscriptions: HashMap::new(),
    })
  }

  pub fn events(&mut self) -> Events<'_> {
    Events { client: self }
  }

  pub fn subscribe(
    &mut self,
    subject: &str,
    queue: Option<&str>,
  ) -> Result<Channel, NatsClientError> {
    check_subject(subject)?;
    let sid = self.sid;
    if let Some(queue) = queue {
      check_queue(queue)?;
    }
    self.connect_if_needed()?;
    let sub = Subscription {
      subject: subject.to_owned(),
      queue: queue.map(|q| q.to_owned()),
    };
    let res = self.subscribe_with_sid(sid, &sub);
    if res.is_ok() {
      self.sid = self.sid.wrapping_add(1);
      self.subscriptions.insert(sid, sub);
    }
    res
  }

  fn subscribe_with_sid(
    &mut self,
    sid: u64,
    sub: &Subscription,
  ) -> Result<Channel, NatsClientError> {
    let cmd = match sub.queue {
      None => format!("SUB {} {}\r\n", sub.subject, sid),
      Some(ref queue) => format!("SUB {} {} {}\r\n", sub.subject, queue, sid),
    };
    self.with_reconnect(|mut state| -> Result<Channel, NatsClientError> {
      state.stream_writer.write_all(cmd.as_bytes())?;
      wait_ok(&mut state)?;
      Ok(Channel { sid })
    })
  }

  fn wait(&mut self) -> Result<Event, NatsClientError> {
    self.connect_if_needed()?;
    self.with_reconnect(|state| -> Result<Event, NatsClientError> {
      let buf_reader = &mut state.buf_reader;
      loop {
        let mut line = String::new();
        match buf_reader.read_line(&mut line) {
          Ok(line_len) if line_len < "PING\r\n".len() => {
            return Err(NatsClientError::from((
              ErrorKind::ServerProtocolError,
              "Incomplete server response",
            )))
          }
          Err(e) => return Err(NatsClientError::from(e)),
          Ok(_) => (),
        }
        if line.starts_with("MSG ") {
          unimplemented!() // TODO
        }
        if line != "PING\r\n" {
          return Err(NatsClientError::from((
            ErrorKind::ServerProtocolError,
            "Server sent an unexpected response",
            line,
          )));
        }
        let cmd = "PONG\r\n";
        state.stream_writer.write_all(cmd.as_bytes())?;
      }
    })
  }

  fn restore_subscriptions(&mut self) -> Result<(), NatsClientError> {
    for (sid, sub) in self.subscriptions.clone() {
      self.subscribe_with_sid(sid, &sub)?;
    }
    Ok(())
  }

  fn with_reconnect<F, T>(&mut self, f: F) -> Result<T, NatsClientError>
  where
    F: Fn(&mut ClientState) -> Result<T, NatsClientError>,
  {
    let mut res: Result<T, NatsClientError> =
      Err(NatsClientError::from((ErrorKind::IoError, "I/O error")));
    for _ in 0..RETRIES_MAX {
      let mut state = self.state.take().unwrap();
      res = match f(&mut state) {
        e @ Err(_) => {
          if let Err(e) = self.reconnect() {
            return Err(e);
          };
          if let Err(e) = self.restore_subscriptions() {
            return Err(NatsClientError::from((
              ClientProtocolError,
              "Failed to restore subscriptions",
              e.description().to_owned(),
            )));
          }
          e
        }
        res @ Ok(_) => {
          self.state = Some(state);
          return res;
        }
      };
    }
    res
  }

  fn reconnect(&mut self) -> Result<(), NatsClientError> {
    if let Some(mut state) = self.state.take() {
      let _ = state.stream_writer.flush();
    }
    self.connect()
  }

  fn connect_if_needed(&mut self) -> Result<(), NatsClientError> {
    if self.state.is_none() {
      self.connect()
    } else {
      Ok(())
    }
  }

  fn connect(&mut self) -> Result<(), NatsClientError> {
    // TODO: circuit_breaker
    self.state = None;
    let servers_count = self.servers_info.len();
    for _ in 0..CIRCUIT_BREAKER_ROUNDS_BEFORE_BREAKING {
      for _ in 0..servers_count {
        let res = self.try_connect();
        if res.is_ok() {
          if self.state.is_none() {
            panic!("Inconsitent state")
          }
          return Ok(());
        } else {
          self.server_idx = (self.server_idx + 1) % servers_count;
        }
      }
      thread::sleep(Duration::from_millis(
        CIRCUIT_BREAKER_WAIT_BETWEEN_ROUNDS_MS,
      ));
    }
    //
    Err(NatsClientError::from((
      ErrorKind::ServerProtocolError,
      "The entire cluster is down or unreachable",
    )))
  }

  fn try_connect(&mut self) -> Result<(), NatsClientError> {
    let server_info = &mut self.servers_info[self.server_idx];
    let stream_reader =
      TcpStream::connect((&server_info.host as &str, server_info.port)).map(stream::Stream::Tcp)?;
    let mut stream_writer = stream_reader.try_clone()?;
    let mut buf_reader = BufReader::new(stream_reader);
    let mut line = String::new();
    match buf_reader.read_line(&mut line) {
      Ok(line_len) if line_len < "INFO {}".len() => {
        return Err(NatsClientError::from(io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unexpect EOF",
        )))
      }
      Err(e) => return Err(NatsClientError::from(e)),
      Ok(_) => {}
    };
    if !line.starts_with("INFO ") {
      return Err(NatsClientError::from(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Server INFO not received",
      )));
    }
    let obj: Value = de::from_str(&line[5..]).or_else(|_| {
      Err(NatsClientError::from(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Invalid JSON object sent by the server",
      )))
    })?;
    // TODO: max_payload/auth/tls
    let connect = ConnectNoCredentials {
      verbose: self.verbose,
      pedantic: true,
      name: "binlogo".to_string(),
    };
    let connect_json = serde_json::to_string(&connect).unwrap();
    let connect_string = format!("CONNECT {}\nPING\n", connect_json);
    let connect_bytes = connect_string.as_bytes();
    stream_writer.write_all(connect_bytes).unwrap();

    println!("CONNECT write done. {}", connect_string);

    if self.verbose {
      let mut line = String::new();
      match buf_reader.read_line(&mut line) {
        Ok(line_len) if line_len != "+OK\r\n".len() => {
          return Err(NatsClientError::from(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unexpected EOF",
          )))
        }
        Err(e) => return Err(NatsClientError::from(e)),
        Ok(_) => {}
      };
      if line != "+OK\r\n" {
        return Err(NatsClientError::from(io::Error::new(
          io::ErrorKind::InvalidInput,
          "Server +OK not received",
        )));
      }
    }

    let mut line = String::new();
    match buf_reader.read_line(&mut line) {
      Ok(line_len) if line_len != "PONG\r\n".len() => {
        println!("Unexpected EOF, {}", line_len);
        return Err(NatsClientError::from(io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unexpected EOF",
        )));
      }
      Err(e) => return Err(NatsClientError::from(e)),
      Ok(_) => (),
    };

    if line != "PONG\r\n" {
      println!("Server PONG not received, but: {}", line);
      return Err(NatsClientError::from(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Server PONG not received",
      )));
    }

    let state = ClientState {
      stream_writer,
      buf_reader,
    };
    self.state = Some(state);
    println!("Connected success");
    Ok(())
  }
}

/// ServerInfo
#[derive(Clone, Debug)]
struct ServerInfo {
  host: String,
  port: u16,
}

#[derive(Serialize, Deserialize)]
struct ConnectNoCredentials {
  verbose: bool,
  pedantic: bool,
  name: String,
}

#[derive(Debug)]
struct ClientState {
  stream_writer: Stream,
  buf_reader: BufReader<Stream>,
}

#[derive(Clone, Debug)]
struct Subscription {
  subject: String,
  queue: Option<String>,
}

pub trait ToStringVec {
  fn to_string_vec(self) -> Vec<String>;
}

impl ToStringVec for &str {
  fn to_string_vec(self) -> Vec<String> {
    vec![self.to_string()]
  }
}

impl ToStringVec for String {
  fn to_string_vec(self) -> Vec<String> {
    vec![self]
  }
}

impl<'a> Iterator for Events<'a> {
  type Item = Event;

  fn next(&mut self) -> Option<Event> {
    let nc = &mut self.client;
    nc.wait().ok().map(|event| event)
  }
}

fn parse_nats_uri(uri: &str) -> Result<Url, NatsClientError> {
  let url = Url::parse(uri)?;
  if url.scheme() != URI_SCHEME {
    Err(NatsClientError::from((
      ErrorKind::InvalidSchemeError,
      "Unsupproted scheme",
    )))
  } else {
    Ok(url)
  }
}

fn check_space(name: &str, errmsg: &'static str) -> Result<(), NatsClientError> {
  if name.contains(" ") {
    Err(NatsClientError::from((
      ErrorKind::ClientProtocolError,
      errmsg,
    )))
  } else {
    Ok(())
  }
}

fn check_subject(subject: &str) -> Result<(), NatsClientError> {
  check_space(subject, "Subject can't contain spaces")
}

fn check_inbox(inbox: &str) -> Result<(), NatsClientError> {
  check_space(inbox, "Inbox name can't contain spaces")
}

fn check_queue(queue: &str) -> Result<(), NatsClientError> {
  check_space(queue, "Queue name can't contain spaces")
}

fn wait_ok(state: &mut ClientState) -> Result<(), NatsClientError> {
  let mut line = String::new();
  match (&mut state.buf_reader).read_line(&mut line) {
    Ok(line_len) if line_len < "OK\r\n".len() => {
      return Err(NatsClientError::from((
        ErrorKind::ServerProtocolError,
        "Incomplete server response",
      )))
    }
    Err(e) => return Err(NatsClientError::from(e)),
    Ok(_) => {}
  };
  match line.as_ref() {
    "+OK\r\n" => Ok(()),
    "PING\r\n" => {
      let pong = b"PONG\r\n";
      state.stream_writer.write_all(pong)?;
      wait_ok(state)
    }
    _ => Err(NatsClientError::from((
      ErrorKind::ServerProtocolError,
      "Received unexpect response from server",
      line,
    ))),
  }
}
