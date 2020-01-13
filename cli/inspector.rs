#![allow(clippy::mutex_atomic)]
#![allow(unused)]

use crate::version::DENO;
use deno_core::InspectorHandle;
use futures::compat::Future01CompatExt;
use futures::Future;
use futures::FutureExt;
use std::net::SocketAddrV4;
use std::pin::Pin;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tokio;
use tokio::task::JoinHandle;
use warp::ws::{Message, WebSocket};
use warp::Filter;
use warp::Future as WarpFuture;
use warp::Sink as WarpSink;
use warp::Stream as WarpStream;

pub struct Inspector {
  enable: bool,
  address: SocketAddrV4,
  // sharable handle to channels passed to isolate
  pub handle: InspectorHandle,
  // sending/receving messages from isolate
  inbound_tx: Arc<Mutex<Sender<String>>>,
  outbound_rx: Arc<Mutex<Receiver<String>>>,
  // signals readiness of inspector
  // TODO(mtharrison): Is a condvar more appropriate?
  ready_tx: Arc<Mutex<Sender<()>>>,
  ready_rx: Arc<Mutex<Receiver<()>>>,
  server_spawn_handle: Option<JoinHandle<Result<(), ()>>>,
  // TODO(mtharrison): Maybe use an atomic bool instead?
  started: Arc<Mutex<bool>>,
  connected: Arc<Mutex<bool>>,
}

impl Inspector {
  pub fn new(enable: bool, address: Option<String>) -> Self {
    let (inbound_tx, inbound_rx) = channel::<String>();
    let (outbound_tx, outbound_rx) = channel::<String>();
    let (ready_tx, ready_rx) = channel::<()>();

    let address = match address {
      Some(address) => address.parse::<SocketAddrV4>().unwrap(),
      None => "127.0.0.1:9888".parse::<SocketAddrV4>().unwrap(),
    };

    Inspector {
      enable,
      address,
      handle: InspectorHandle::new(outbound_tx, inbound_rx),
      inbound_tx: Arc::new(Mutex::new(inbound_tx)),
      outbound_rx: Arc::new(Mutex::new(outbound_rx)),
      ready_rx: Arc::new(Mutex::new(ready_rx)),
      ready_tx: Arc::new(Mutex::new(ready_tx)),
      server_spawn_handle: None,
      started: Arc::new(Mutex::new(false)),
      connected: Arc::new(Mutex::new(false)),
    }
  }

  pub fn serve(&self) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send>> {
    let state = ClientState {
      sender: self.inbound_tx.clone(),
      receiver: self.outbound_rx.clone(),
      ready_tx: self.ready_tx.clone(),
      connected: self.connected.clone(),
    };

    let websocket = warp::path("websocket").and(warp::ws2()).map(
      enclose!((state) move |ws: warp::ws::Ws2| {
        ws.on_upgrade(enclose!((state) move |socket| {
          let client = Client::new(state, socket);
          client.on_connection()
        }))
      }),
    );

    // todo(matt): Make this unique per run (https://github.com/denoland/deno/pull/2696#discussion_r309282566)
    let uuid = "97690037-256e-4e27-add0-915ca5421e2f";

    let ip = format!("{}", self.address.ip());
    let port = self.address.port();

    let response_json = json!([{
      "description": "deno",
      "devtoolsFrontendUrl": format!("chrome-devtools://devtools/bundled/js_app.html?experiments=true&v8only=true&ws={}:{}/websocket", ip, port),
      "devtoolsFrontendUrlCompat": format!("chrome-devtools://devtools/bundled/inspector.html?experiments=true&v8only=true&ws={}:{}/websocket", ip, port),
      "faviconUrl": "https://www.deno-play.app/images/deno.svg",
      "id": uuid,
      "title": format!("deno[{}]", std::process::id()),
      "type": "deno",
      "url": "file://",
      "webSocketDebuggerUrl": format!("ws://{}:{}/websocket", ip, port)
    }]);

    let response_version = json!({
      "Browser": format!("Deno/{}", DENO),
      "Protocol-Version": "1.1",
      "webSocketDebuggerUrl": format!("ws://{}:{}/{}", ip, port, uuid)
    });

    let json =
      warp::path("json").map(move || warp::reply::json(&response_json));

    let version = path!("json" / "version")
      .map(move || warp::reply::json(&response_version));

    let routes = websocket.or(version).or(json);

    // Unfortunately warp still uses 0.1 futures, so need to
    // call .compat() here
    warp::serve(routes).bind(self.address).compat().boxed()
  }

  pub fn start(&mut self) {
    let mut started = self.started.lock().unwrap();

    if *started || !self.enable {
      return;
    }

    *started = true;

    let fut = self.serve();
    let join_handle = tokio::spawn(fut);
    self.server_spawn_handle = Some(join_handle);

    eprintln!(
      "Debugger listening on ws://{}:{}/97690037-256e-4e27-add0-915ca5421e2f",
      self.address.ip(),
      self.address.port(),
    );

    self
      .ready_rx
      .lock()
      .unwrap()
      .recv()
      .expect("Error waiting for inspector server to start.");
    // TODO(mtharrison): This is to allow v8 to ingest some inspector messages - find a more reliable way
    std::thread::sleep(std::time::Duration::from_secs(1));
    println!("Inspector frontend connected.");
  }
}

impl Drop for Inspector {
  fn drop(&mut self) {
    if *self.connected.lock().unwrap() {
      println!("Waiting for debugger to disconnect...");
    }
  }
}

#[derive(Clone)]
pub struct ClientState {
  sender: Arc<Mutex<Sender<String>>>,
  receiver: Arc<Mutex<Receiver<String>>>,
  ready_tx: Arc<Mutex<Sender<()>>>,
  connected: Arc<Mutex<bool>>,
}

pub struct Client {
  state: ClientState,
  socket: WebSocket,
}

impl Client {
  fn new(state: ClientState, socket: WebSocket) -> Client {
    Client { state, socket }
  }

  fn on_connection(self) -> impl WarpFuture<Item = (), Error = ()> {
    let socket = self.socket;
    let sender = self.state.sender;
    let receiver = self.state.receiver;
    let connected = self.state.connected;
    let ready_tx = self.state.ready_tx;

    let (mut user_ws_tx, user_ws_rx) = socket.split();

    let fut_rx = user_ws_rx
      .for_each(move |msg| {
        let msg_str = msg.to_str().unwrap();
        sender
          .lock()
          .unwrap()
          .send(msg_str.to_owned())
          .unwrap_or_else(|err| println!("Err: {}", err));
        Ok(())
      })
      .map_err(|_| {});

    // TODO(mtharrison): This is a mess. There must be a better way to do this - maybe use async channels or wrap them with a stream?

    std::thread::spawn(move || {
      let receiver = receiver.lock().unwrap();
      loop {
        let received = receiver.recv();
        if let Ok(msg) = received {
          let _ = ready_tx.lock().unwrap().send(());
          *connected.lock().unwrap() = true;
          user_ws_tx = user_ws_tx.send(Message::text(msg)).wait().unwrap();
        }
      }
    });

    fut_rx
  }
}
