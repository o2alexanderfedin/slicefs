//! FUSET-05: a signal reaches the stop callback (its own test binary, because
//! signal handlers are global to the process).

use std::sync::mpsc;
use std::time::Duration;

#[test]
fn a_signal_calls_the_stop_callback_and_ignored_signals_stay_ignored() {
    // An ignored signal (nohup, SIGINT in a background job) stays ignored.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };

    let (tx, rx) = mpsc::channel();
    slicefs_cli::signals::install(move |sig| {
        tx.send(sig).unwrap();
        true
    })
    .unwrap();

    unsafe { libc::raise(libc::SIGHUP) };
    unsafe { libc::raise(libc::SIGTERM) };
    let got = rx.recv_timeout(Duration::from_secs(5)).expect("the stop callback did not run");
    assert_eq!(got, libc::SIGTERM, "SIGHUP was ignored at start, so SIGTERM comes first");
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "the callback runs once");
}
