//! The socket side of the IDE server: accepting connections from the `claude`
//! CLI and moving text frames between them and [`ClaudeCode`] on the main
//! thread.

use crate::{ClaudeCode, ConnectionId};
use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::{
    self, Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
};
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{AsyncApp, WeakEntity};
use smol::net::{TcpListener, TcpStream};
use std::{net::Ipv4Addr, sync::Arc};

/// The header the CLI sends its lock file's `authToken` in.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
const SUBPROTOCOL_HEADER: &str = "sec-websocket-protocol";
const MCP_SUBPROTOCOL: &str = "mcp";

/// Binds a port on the loopback interface and serves connections until the
/// returned future is dropped.
pub(crate) async fn run(
    this: WeakEntity<ClaudeCode>,
    auth_token: Arc<str>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("binding the Claude Code IDE server")?;
    let port = listener.local_addr()?.port();
    this.update(cx, |this, cx| this.server_started(port, cx))?;

    loop {
        let (stream, _) = listener.accept().await?;
        let (incoming_tx, mut incoming_rx) = mpsc::unbounded::<String>();
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded::<String>();
        let connection_id = this.update(cx, |this, _| this.add_connection(outgoing_tx))?;
        let io = cx.background_executor().spawn(handle_io(
            stream,
            auth_token.clone(),
            incoming_tx,
            outgoing_rx,
        ));

        let this = this.clone();
        cx.spawn(async move |cx| {
            while let Some(message) = incoming_rx.next().await {
                this.update(cx, |this, cx| this.handle_message(connection_id, message, cx))
                    .ok();
            }
            if let Err(error) = io.await {
                log::info!("Claude Code connection closed: {error:#}");
            }
            this.update(cx, |this, cx| this.remove_connection(connection_id, cx))
                .ok();
        })
        .detach();
    }
}

async fn handle_io(
    stream: TcpStream,
    auth_token: Arc<str>,
    incoming_tx: mpsc::UnboundedSender<String>,
    mut outgoing_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // The error type is tungstenite's handshake callback signature.
    #[allow(clippy::result_large_err)]
    let authorize = move |request: &Request, mut response: Response| {
        let authorized = request
            .headers()
            .get(AUTH_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|token| token == &*auth_token);
        if authorized {
            // MCP clients ask for the `mcp` subprotocol and hang up on a
            // server that doesn't agree to it.
            let wants_mcp = request
                .headers()
                .get_all(SUBPROTOCOL_HEADER)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .any(|protocol| protocol.trim() == MCP_SUBPROTOCOL);
            if wants_mcp {
                response.headers_mut().insert(
                    SUBPROTOCOL_HEADER,
                    tungstenite::http::HeaderValue::from_static(MCP_SUBPROTOCOL),
                );
            }
            Ok(response)
        } else {
            let mut error = ErrorResponse::new(Some("Unauthorized".to_string()));
            *error.status_mut() = StatusCode::UNAUTHORIZED;
            Err(error)
        }
    };
    let socket = async_tungstenite::accept_hdr_async(stream, authorize).await?;
    let (mut sink, mut stream) = socket.split();

    loop {
        futures::select_biased! {
            outgoing = outgoing_rx.next() => match outgoing {
                Some(text) => sink.send(Message::text(text)).await?,
                // Zed closed the connection, e.g. because the server was turned off.
                None => break,
            },
            incoming = stream.next().fuse() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    incoming_tx.unbounded_send(text.to_string())?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                // Pings are answered by tungstenite itself.
                Some(Ok(_)) => {}
                Some(Err(tungstenite::Error::ConnectionClosed)) => break,
                Some(Err(error)) => return Err(error.into()),
            },
        }
    }

    sink.close(None).await.ok();
    Ok(())
}

impl ConnectionId {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}
