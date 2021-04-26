use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::io::{BufReader, Error, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::command::Command;
use crate::resp::{RedisProtocolParser, RESP};
use bus::Bus;

mod command;
mod resp;

#[repr(C)]
pub struct RedisLess {
    data_mapper: HashMap<Vec<u8>, DataType>,
    string_store: HashMap<Vec<u8>, Vec<u8>>,
}

enum DataType {
    String,
    List,
    Set,
    Hash,
}

impl RedisLess {
    pub fn new() -> Self {
        RedisLess {
            data_mapper: HashMap::new(),
            string_store: HashMap::new(),
        }
    }

    pub fn set(&mut self, key: &[u8], value: &[u8]) {
        self.data_mapper.insert(key.to_vec(), DataType::String);
        self.string_store.insert(key.to_vec(), value.to_vec());
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.string_store.get(key).map(|v| &v[..])
    }

    pub fn del(&mut self, key: &[u8]) -> u32 {
        match self.data_mapper.get(key) {
            Some(data_type) => match data_type {
                DataType::String => match self.string_store.remove(key) {
                    Some(_) => 1,
                    None => 0,
                },
                DataType::List => 0,
                DataType::Set => 0,
                DataType::Hash => 0,
            },
            None => 0,
        }
    }
}

fn handle_request(redisless: &mut RedisLess, mut stream: &TcpStream) {
    let mut buf_reader = BufReader::new(stream);
    let mut buf = [0; 512];

    while let Ok(s) = buf_reader.read(&mut buf) {
        if s < 512 {
            break;
        }
    }

    match RedisProtocolParser::parse(&buf) {
        Ok((resp, _)) => match resp {
            RESP::Array(x) => match Command::parse(x) {
                Command::Set(k, v) => {
                    redisless.set(k.as_slice(), v.as_slice());
                    stream.write(b"+OK\r\n");
                    return;
                }
                Command::Get(k) => {
                    if let Some(value) = redisless.get(k.as_slice()) {
                        stream.write(
                            format!("+{}\r\n", std::str::from_utf8(value).unwrap()).as_bytes(),
                        );
                        return;
                    };

                    stream.write(b"-ERR key does not exist\r\n");
                    return;
                }
                Command::Del(k) => {
                    let total_del = redisless.del(k.as_slice());
                    stream.write(format!(":{}\r\n", total_del).as_bytes());
                    return;
                }
                Command::NotSupported(m) => {
                    stream.write(format!("-ERR {}\r\n", m).as_bytes());
                    return;
                }
                Command::Error(m) => {
                    stream.write(format!("-ERR {}\r\n", m).as_bytes());
                    return;
                }
            },
            _ => {}
        },
        _ => {}
    };

    stream.write(b"+OK\r\n");
}

#[repr(C)]
pub struct Server {
    send_state_ch: Sender<ServerState>,
    recv_state_ch: Receiver<ServerState>,
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum ServerState {
    Start,
    Started,
    Stop,
    Stopped,
    Timeout,
    Error(String),
}

impl Server {
    pub fn new(redisless: RedisLess, port: u32) -> Self {
        let (send_state_ch, recv_state_ch) = unbounded::<ServerState>();
        let mut bus: Bus<ServerState> = Bus::new(100);

        let s = Server {
            send_state_ch,
            recv_state_ch,
        };

        // TODO export conf
        s._init_configuration(format!("127.0.0.1:{}", port), redisless);

        s
    }

    pub fn _init_configuration<A: Into<String>>(&self, addr: A, redisless: RedisLess) {
        let addr = addr.into();
        let state_send = self.send_state_ch.clone();
        let start_state_recv = self.recv_state_ch.clone();
        let stop_state_recv = self.recv_state_ch.clone();

        let stop_server = Arc::new(AtomicBool::new(false));
        let stop_server_th2 = stop_server.clone();

        thread::spawn(move || {
            let addr = addr;
            let mut redisless = redisless;

            for server_state in start_state_recv {
                if server_state == ServerState::Start {
                    match TcpListener::bind(addr.as_str()) {
                        Ok(listener) => {
                            // notify that the server has been started
                            let _ = state_send.send(ServerState::Started);

                            // listen incoming requests
                            for stream in listener.incoming() {
                                match stream {
                                    Ok(tcp_stream) => {
                                        while !(*stop_server).load(Ordering::Relaxed) {
                                            // TODO support graceful shutdown
                                            handle_request(redisless.borrow_mut(), &tcp_stream);
                                        }
                                    }
                                    Err(err) => {
                                        state_send.send(ServerState::Error(format!("{:?}", err)));
                                    }
                                }
                            }

                            // notify that the server has been stopped
                            let _ = state_send.send(ServerState::Stopped);
                        }
                        Err(err) => {
                            state_send.send(ServerState::Error(format!("{:?}", err)));
                        }
                    };
                }
            }
        });

        thread::spawn(move || {
            // listen stop server signal
            for server_state in stop_state_recv {
                if server_state == ServerState::Stop {
                    stop_server_th2.store(true, Ordering::Relaxed);
                }
            }
        });
    }

    fn change_state(&self, change_to: ServerState) -> ServerState {
        let send_state_ch = self.send_state_ch.clone();

        let post_change_to_state = match change_to {
            ServerState::Start => Some(ServerState::Started),
            ServerState::Stop => Some(ServerState::Stopped),
            ServerState::Started
            | ServerState::Stopped
            | ServerState::Timeout
            | ServerState::Error(_) => None,
        };

        let change_to_th1 = change_to.clone();

        let _ = thread::spawn(move || {
            let _ = thread::sleep(Duration::from_millis(1000));
            let _ = send_state_ch.send(change_to_th1);
        });

        // wait for changing state
        for server_state in self.recv_state_ch.recv_timeout(Duration::from_secs(10)) {
            if Some(&server_state) == post_change_to_state.as_ref() {
                return server_state;
            }
        }

        ServerState::Timeout
    }

    /// start server
    pub fn start(&self) -> ServerState {
        self.change_state(ServerState::Start)
    }

    /// stop server
    pub fn stop(&self) -> ServerState {
        self.change_state(ServerState::Stop)
    }
}

#[no_mangle]
pub extern "C" fn redisless_new() -> *mut RedisLess {
    Box::into_raw(Box::new(RedisLess::new()))
}

#[no_mangle]
pub extern "C" fn redisless_server_new(redisless: RedisLess) -> *const Server {
    Box::into_raw(Box::new(Server::new(redisless, 16379)))
}

#[no_mangle]
pub extern "C" fn redisless_server_start(server: &Server) {
    server.start();
}

#[no_mangle]
pub extern "C" fn redisless_server_stop(server: &Server) {
    server.stop();
}

#[cfg(test)]
mod tests {
    use redis::{Commands, RedisResult};

    use crate::{RedisLess, Server, ServerState};

    #[test]
    fn test_set_get_and_del() {
        let mut redisless = RedisLess::new();
        redisless.set(b"key", b"xxx");
        assert_eq!(redisless.get(b"key"), Some(&b"xxx"[..]));
        assert_eq!(redisless.del(b"key"), 1);
        assert_eq!(redisless.del(b"key"), 0);
        assert_eq!(redisless.get(b"does not exist"), None);
    }

    #[test]
    fn test_redis_implementation() {
        let server = Server::new(RedisLess::new(), 16379);

        assert_eq!(server.start(), ServerState::Started);

        let redis_client = redis::Client::open("redis://127.0.0.1:16379/").unwrap();
        let mut con = redis_client.get_connection().unwrap();

        let _: () = con.set("key", "value").unwrap();
        let x: String = con.get("key").unwrap();
        assert_eq!(x, "value");

        let x: RedisResult<String> = con.get("not-existing-key");
        assert_eq!(x.is_err(), true);

        let x: u32 = con.del("key").unwrap();
        assert_eq!(x, 1);

        let x: u32 = con.del("key").unwrap();
        assert_eq!(x, 0);

        let _: () = con.set("key2", "value2").unwrap();
        let x: String = con.get("key2").unwrap();
        assert_eq!(x, "value2");

        assert_eq!(server.stop(), ServerState::Stopped);
    }
}
