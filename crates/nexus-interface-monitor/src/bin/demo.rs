//! Demo driver. Spawns the Interface Monitor, subscribes to every
//! `NexusEvent`, and prints them to stdout until Ctrl-C.
//!
//! Not a production binary — intended for eyeballing on a real
//! machine per DD-001 Phase 6's exit criterion.

use std::error::Error;

use nexus_interface_monitor::spawn_interface_monitor;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn Error>> {
    let (tx, mut rx) = broadcast::channel(256);
    let shutdown = CancellationToken::new();

    println!("starting interface monitor; Ctrl-C to stop");
    let (_cmd_tx, cmd_rx) = nexus_interface_monitor::command_channel();
    let monitor_handle =
        spawn_interface_monitor(tx, shutdown.clone(), cmd_rx).await?;

    let printer = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    res = rx.recv() => match res {
                        Ok(event) => println!("{event:?}"),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("receiver lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        })
    };

    tokio::signal::ctrl_c().await?;
    eprintln!("\ninterrupt received, shutting down");
    shutdown.cancel();

    match monitor_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("monitor task error: {e}"),
        Err(e) => eprintln!("monitor task join error: {e}"),
    }
    let _ = printer.await;
    Ok(())
}
