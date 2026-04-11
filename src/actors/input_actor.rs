//! crossterm event-stream -> AppMsg pump.
//!
//! Owns the keyboard. The only place in the codebase allowed to read from
//! crossterm. Runs as a tokio task, forwards key/resize events to the app
//! via an mpsc channel.

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::events::AppMsg;

pub fn spawn(tx: mpsc::Sender<AppMsg>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(evt) = stream.next().await {
            let msg = match evt {
                Ok(Event::Key(k)) => AppMsg::Key(k),
                Ok(Event::Resize(w, h)) => AppMsg::Resize(w, h),
                Ok(Event::FocusGained) | Ok(Event::FocusLost) => continue,
                Ok(Event::Mouse(_)) | Ok(Event::Paste(_)) => continue,
                Err(e) => {
                    let _ = tx.send(AppMsg::Fatal(format!("input stream: {}", e))).await;
                    break;
                }
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    })
}
