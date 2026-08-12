//! Serving on a real socket: startup hooks, an in-flight request that survives
//! a shutdown signal, and shutdown hooks that run after it drains.
//!
//! Every other test in this suite drives the composed `tower::Service` in
//! process, which is faster and proves everything except the part that only
//! exists once there is a listener. This file binds port 0 and speaks HTTP/1.1
//! down a socket, because "graceful shutdown drains in-flight work" is a claim
//! about the accept loop and cannot be made anywhere else.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use moso::deps::tokio;
use moso::prelude::*;
use moso::response::Text;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Notify;

mod support;

/// Nothing in this file may hang the suite; every wait is bounded by this.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

/// A rendezvous between the test and the handler.
///
/// The handler announces that it has started and then blocks until the test
/// lets it go. That is what makes "in flight" a fact rather than a race against
/// a sleep.
#[derive(Debug, Default)]
pub struct Gate {
    /// Fired by the handler when it begins.
    started: Notify,
    /// Fired by the test when the handler may finish.
    release: Notify,
}

/// Records what ran, so the hook assertions are not about timing.
#[derive(Debug, Default)]
pub struct Journal {
    /// How many startup hooks ran.
    startup: AtomicU32,
    /// How many shutdown hooks ran.
    shutdown: AtomicU32,
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

/// Answer at once.
#[endpoint]
async fn quick() -> Result<Text> {
    Ok(Text("quick".to_owned()))
}

/// Announce, wait to be released, then answer.
#[endpoint]
async fn slow(Inject(gate): Inject<Gate>) -> Result<Text> {
    gate.started.notify_one();
    gate.release.notified().await;
    Ok(Text("finished".to_owned()))
}

/// Panic, to prove the accept loop outlives one bad request.
#[endpoint]
async fn boom() -> Result<Text> {
    panic!("a deliberate panic, on a real connection");
}

fn router() -> Router {
    moso::routes! {
        GET "/quick" => quick,
        GET "/slow"  => slow,
        GET "/boom"  => boom,
    }
}

// ---------------------------------------------------------------------------
// Raw HTTP over the socket
// ---------------------------------------------------------------------------

/// A parsed HTTP/1.1 response: the status line's code and everything after the
/// blank line.
struct Raw {
    status: u16,
    body: String,
}

/// Write a `GET` with `Connection: close`, so the server ends the response by
/// closing and the reader can simply read to EOF.
async fn write_get(stream: &mut TcpStream, host: &str, path: &str) {
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: */*\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request goes out");
    stream.flush().await.expect("flushed");
}

/// Read to EOF and split the head from the body.
async fn read_response(mut stream: TcpStream) -> Raw {
    let mut text = String::new();
    stream
        .read_to_string(&mut text)
        .await
        .expect("the response comes back");

    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("not an HTTP response: {text:?}"));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?}"));

    Raw {
        status,
        body: body.to_owned(),
    }
}

/// Bind port 0 and start serving, returning the address, the shutdown signal,
/// and the join handle for the serve task.
async fn serve(
    application: moso::App,
) -> (
    String,
    moso::Signal,
    tokio::task::JoinHandle<moso::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port");
    let address = listener.local_addr().expect("bound").to_string();
    let signal = application.shutdown_signal();
    let serving = tokio::spawn(application.serve_on(listener));
    (address, signal, serving)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_is_served_over_a_real_socket() {
    let gate = Arc::new(Gate::default());
    let application = App::new(Cfg::default())
        .provide_arc(Arc::clone(&gate))
        .mount(router())
        .build()
        .expect("builds");
    let (address, signal, serving) = serve(application).await;

    let mut stream = TcpStream::connect(&address).await.expect("connected");
    write_get(&mut stream, &address, "/quick").await;
    let reply = tokio::time::timeout(PATIENCE, read_response(stream))
        .await
        .expect("the server answered in time");

    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, "quick");

    signal.trigger();
    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("the server stopped in time")
        .expect("the serve task did not panic")
        .expect("serving ended cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_a_request_that_is_already_in_flight() {
    let gate = Arc::new(Gate::default());
    let application = App::new(Cfg::default())
        .provide_arc(Arc::clone(&gate))
        .mount(router())
        .build()
        .expect("builds");
    let (address, signal, serving) = serve(application).await;

    // 1. Get a request into the handler and leave it there.
    let mut stream = TcpStream::connect(&address).await.expect("connected");
    write_get(&mut stream, &address, "/slow").await;
    tokio::time::timeout(PATIENCE, gate.started.notified())
        .await
        .expect("the handler started");

    // 2. Signal shutdown while it is still running. This is the moment a
    //    non-graceful server drops the connection and the client sees a reset.
    signal.trigger();

    // 3. Give the shutdown a moment to do the wrong thing if it is going to.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(signal.is_shutting_down());

    // 4. Let the handler finish, and read what the client actually got.
    gate.release.notify_one();
    let reply = tokio::time::timeout(PATIENCE, read_response(stream))
        .await
        .expect("the in-flight request completed");

    assert_eq!(
        reply.status, 200,
        "an in-flight request must be finished, not cut off"
    );
    assert_eq!(reply.body, "finished");

    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("the server stopped after draining")
        .expect("the serve task did not panic")
        .expect("serving ended cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_on_one_connection_does_not_take_the_server_with_it() {
    let gate = Arc::new(Gate::default());
    let application = App::new(Cfg::default())
        .provide_arc(Arc::clone(&gate))
        .mount(router())
        // Explicit, because the panic message *is* disclosed under `dev` and
        // this test is about the deployed shape.
        .profile(moso::config::Profile::Production)
        .build()
        .expect("builds");
    let (address, signal, serving) = serve(application).await;

    // In process, `catch_panic` turns this into a 500. Over a socket there is
    // a second thing to prove: the accept loop is still there afterwards.
    let mut stream = TcpStream::connect(&address).await.expect("connected");
    write_get(&mut stream, &address, "/boom").await;
    let panicked = tokio::time::timeout(PATIENCE, read_response(stream))
        .await
        .expect("the panic was answered, not dropped");
    assert_eq!(panicked.status, 500, "{}", panicked.body);
    assert!(
        !panicked.body.contains("a deliberate panic"),
        "and it was not narrated to the client: {}",
        panicked.body
    );

    // A fresh connection, after the panic.
    let mut stream = TcpStream::connect(&address).await.expect("still listening");
    write_get(&mut stream, &address, "/quick").await;
    let after = tokio::time::timeout(PATIENCE, read_response(stream))
        .await
        .expect("the next request is served");
    assert_eq!(after.status, 200, "{}", after.body);
    assert_eq!(after.body, "quick");

    signal.trigger();
    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("stopped in time")
        .expect("no panic escaped into the serve task")
        .expect("clean");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listener_stops_accepting_once_the_signal_fires() {
    let gate = Arc::new(Gate::default());
    let application = App::new(Cfg::default())
        .provide_arc(Arc::clone(&gate))
        .mount(router())
        .build()
        .expect("builds");
    let (address, signal, serving) = serve(application).await;

    // Prove the port is live before asking it to stop.
    let mut stream = TcpStream::connect(&address).await.expect("connected");
    write_get(&mut stream, &address, "/quick").await;
    assert_eq!(read_response(stream).await.status, 200);

    signal.trigger();
    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("the server stopped in time")
        .expect("no panic")
        .expect("clean");

    // The listener is closed, so a connection either fails outright or is
    // accepted by the OS backlog and then never answered. Both are "not
    // serving"; what must not happen is a 200.
    let attempt = tokio::time::timeout(Duration::from_millis(500), async {
        let mut stream = TcpStream::connect(&address).await?;
        write_get(&mut stream, &address, "/quick").await;
        let mut text = String::new();
        stream.read_to_string(&mut text).await?;
        Ok::<String, std::io::Error>(text)
    })
    .await;

    match attempt {
        Err(_elapsed) => {}
        Ok(Err(_refused)) => {}
        Ok(Ok(text)) => assert!(
            !text.contains("200 OK"),
            "the socket answered after shutdown: {text:?}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_and_shutdown_hooks_run_in_the_documented_order() {
    let journal = Arc::new(Journal::default());

    let application = App::new(Cfg::default())
        .provide_arc(Arc::clone(&journal))
        .mount(router())
        .provide(Gate::default())
        .on_startup({
            let journal = Arc::clone(&journal);
            move |_resolver| {
                let journal = Arc::clone(&journal);
                async move {
                    journal.startup.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }
        })
        .on_shutdown({
            let journal = Arc::clone(&journal);
            move |_resolver| {
                let journal = Arc::clone(&journal);
                async move {
                    journal.shutdown.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .build()
        .expect("builds");
    let (address, signal, serving) = serve(application).await;

    // The startup hook has run by the time the port answers: `serve_on` runs
    // hooks *before* it binds, so a request proves it.
    let mut stream = TcpStream::connect(&address).await.expect("connected");
    write_get(&mut stream, &address, "/quick").await;
    assert_eq!(read_response(stream).await.status, 200);
    assert_eq!(journal.startup.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal.shutdown.load(Ordering::SeqCst),
        0,
        "a shutdown hook must not run while the application is serving"
    );

    signal.trigger();
    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("stopped in time")
        .expect("no panic")
        .expect("clean");

    assert_eq!(
        journal.shutdown.load(Ordering::SeqCst),
        1,
        "the shutdown hook has to run before `serve_on` returns"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_startup_hook_stops_the_boot() {
    let application = App::new(Cfg::default())
        .provide(Gate::default())
        .mount(router())
        .on_startup(|_resolver| async { Err(Error::internal_msg("the migration did not apply")) })
        .build()
        .expect("the application itself is fine");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("bound").to_string();

    let error = tokio::time::timeout(PATIENCE, application.serve_on(listener))
        .await
        .expect("it gave up promptly")
        .expect_err("the hook failed, so serving must not begin");
    assert!(
        error.to_string().contains("migration"),
        "the hook's own error has to survive: {error}"
    );

    // Nothing is listening on that port any more.
    let refused = tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(&address))
        .await
        .expect("the connect attempt resolved");
    assert!(
        refused.is_err(),
        "a failed startup must not leave a listener behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_holds_the_shutdown_open_for_a_background_guard() {
    let application = App::new(Cfg::default())
        .provide(Gate::default())
        .mount(router())
        .build()
        .expect("builds");

    let drain = application
        .resolver()
        .get::<moso::shutdown::Drain>()
        .expect("the framework provides its own drain");
    let signal = application.shutdown_signal();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let serving = tokio::spawn(application.serve_on(listener));

    // Work that outlives a request: an outbox flush, a webhook retry.
    let guard = drain.guard("outbox");
    assert_eq!(drain.outstanding(), 1);

    signal.trigger();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !serving.is_finished(),
        "the drain must hold the process open while the guard is alive"
    );

    drop(guard);
    tokio::time::timeout(PATIENCE, serving)
        .await
        .expect("released, so it finishes")
        .expect("no panic")
        .expect("clean");
    assert_eq!(drain.outstanding(), 0);
}
