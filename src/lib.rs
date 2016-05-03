extern crate mio;
extern crate jsonrpc_core;
extern crate bytes;
extern crate slab;

use mio::*;
use mio::unix::*;
use bytes::{Buf, ByteBuf, MutByteBuf, SliceBuf};
use std::path::PathBuf;
use std::io;
use jsonrpc_core::IoHandler;
use std::sync::*;

const SERVER: Token = Token(0);
const CLIENT: Token = Token(1);

struct SocketConnection {
    socket: UnixStream,
    buf: Option<ByteBuf>,
    mut_buf: Option<MutByteBuf>,
    token: Option<Token>,
    interest: EventSet,
}

type Slab<T> = slab::Slab<T, Token>;

impl SocketConnection {
    fn new(sock: UnixStream) -> Self {
        SocketConnection {
            socket: sock,
            buf: None,
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: None,
            interest: EventSet::hup(),
        }
    }

    fn writable(&mut self, event_loop: &mut EventLoop<RpcServer>, handler: &IoHandler) -> io::Result<()> {
        let mut buf = self.buf.take().unwrap();

        match self.socket.try_write_buf(&mut buf) {
            Ok(None) => {
                self.buf = Some(buf);
                self.interest.insert(EventSet::writable());
            },
            Ok(Some(r)) => {
                self.mut_buf = Some(buf.flip());
                self.interest.insert(EventSet::readable());
                self.interest.remove(EventSet::writable());
            },
            Err(e) => {
                //warn!(target: "ipc", "Error sending data: {:?}", e);
                //::std::io::Error::last_os_error()
            },
        }

        event_loop.reregister(&self.socket, self.token.unwrap(), self.interest, PollOpt::edge() | PollOpt::oneshot())
    }

    fn readable(&mut self, event_loop: &mut EventLoop<RpcServer>, handler: &IoHandler) -> io::Result<()> {
        let mut buf = self.mut_buf.take().unwrap();

        match self.socket.try_read_buf(&mut buf) {
            Ok(None) => {
                self.mut_buf = Some(buf);
            }
            Ok(Some(r)) => {

                String::from_utf8(buf.bytes().to_vec())
                    .map(|rpc_msg| {
                        let response: Option<String> = handler.handle_request(&rpc_msg);
                        if let Some(response_str) = response {
                            let response_bytes = response_str.into_bytes();
                            self.buf = Some(ByteBuf::from_slice(&response_bytes));
                        }
                    });

                self.interest.remove(EventSet::readable());
                self.interest.insert(EventSet::writable());
            }
            Err(e) => {
                //warn!(target: "ipc", "Error receiving data: {:?}", e);
                self.interest.remove(EventSet::readable());
            }

        };

        event_loop.reregister(&self.socket, self.token.unwrap(), self.interest, PollOpt::edge() | PollOpt::oneshot())
    }
}

struct RpcServer {
    socket: UnixListener,
    connections: Slab<SocketConnection>,
    io_handler: Arc<IoHandler>,
}

struct Server {
    rpc_server: RwLock<RpcServer>,
    event_loop: RwLock<EventLoop<RpcServer>>,
}

impl Server {
    fn new(socket_addr: &str, io_handler: &Arc<IoHandler>) -> Server {
        let (server, event_loop) = RpcServer::start(socket_addr, io_handler);
        Server {
            rpc_server: RwLock::new(server),
            event_loop: RwLock::new(event_loop),
        }
    }

    fn run(&self) {
        let mut event_loop = self.event_loop.write().unwrap();
        let mut server = self.rpc_server.write().unwrap();
        event_loop.run(&mut server);
    }

    fn poll(&self) {
        let mut event_loop = self.event_loop.write().unwrap();
        let mut server = self.rpc_server.write().unwrap();

        event_loop.run_once(&mut server, Some(100));
    }
}

impl RpcServer {

    /// start ipc rpc server (blocking)
    pub fn start(addr: &str, io_handler: &Arc<IoHandler>) -> (RpcServer, EventLoop<RpcServer>) {
        let mut event_loop = EventLoop::new().unwrap();
        ::std::fs::remove_file(addr); // ignore error (if no file)
        let socket = UnixListener::bind(&addr).unwrap();
        event_loop.register(&socket, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
        let mut server = RpcServer {
            socket: socket,
            connections: Slab::new_starting_at(Token(1), 8),
            io_handler: io_handler.clone(),
        };
        (server, event_loop)
    }

    fn accept(&mut self, event_loop: &mut EventLoop<RpcServer>) -> io::Result<()> {
        let new_client_socket = self.socket.accept().unwrap().unwrap();
        let connection = SocketConnection::new(new_client_socket);
        let token = self.connections.insert(connection).ok().expect("fatal: Could not add connectiont o slab (memory issue?)");

        self.connections[token].token = Some(token);
        event_loop.register(
            &self.connections[token].socket,
            token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot()
        ).ok().expect("could not register socket with event loop (memory issue?)");

        Ok(())
    }

    fn connection_readable(&mut self, event_loop: &mut EventLoop<RpcServer>, tok: Token) -> io::Result<()> {
        let io_handler = self.io_handler.clone();
        self.connection(tok).readable(event_loop, &io_handler)
    }

    fn connection_writable(&mut self, event_loop: &mut EventLoop<RpcServer>, tok: Token) -> io::Result<()> {
        let io_handler = self.io_handler.clone();
        self.connection(tok).writable(event_loop, &io_handler)
    }

    fn connection<'a>(&'a mut self, tok: Token) -> &'a mut SocketConnection {
        &mut self.connections[tok]
    }
}

impl Handler for RpcServer {
    type Timeout = usize;
    type Message = ();

    fn ready(&mut self, event_loop: &mut EventLoop<RpcServer>, token: Token, events: EventSet) {
        if events.is_readable() {
            match token {
                SERVER => self.accept(event_loop).unwrap(),
                _ => self.connection_readable(event_loop, token).unwrap()
            };
        }

        if events.is_writable() {
            match token {
                SERVER => { },
                _ => self.connection_writable(event_loop, token).unwrap()
            };
        }
    }
}

#[cfg(test)]
fn dummy_request(addr: &str, buf: &[u8]) -> Vec<u8> {
    use std::io::{Read, Write};

    let mut poll = Poll::new().unwrap();
    let mut sock = UnixStream::connect(addr).unwrap();
    poll.register(&sock, Token(0), EventSet::writable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    poll.poll(Some(500));
    sock.write(buf);
    poll.reregister(&sock, Token(0), EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    poll.poll(Some(500));
    let mut buf = Vec::new();
    sock.read(&mut buf);
    buf
}

#[test]
pub fn test_reqrep() {
    use std::sync::Arc;
    use jsonrpc_core::*;

    struct SayHello;
    impl MethodCommand for SayHello {
        fn execute(&self, _params: Params) -> Result<Value, Error> {
            Ok(Value::String("hello".to_string()))
        }
    }

    let addr = "/tmp/test.ipc";
    let io = IoHandler::new();
    io.add_method("say_hello", SayHello);
    let server = Server::new(addr, &Arc::new(io));

    std::thread::spawn(move || {
        server.run()
    });


    let request = r#"{"jsonrpc": "2.0", "method": "say_hello", "params": [42, 23], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","result":"hello","id":1}"#;
    assert_eq!(String::from_utf8(dummy_request(addr, request.as_bytes())).unwrap(), response.to_string());
}