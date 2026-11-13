//! One push-to-talk press: stream audio over the WebSocket, show partials, type finals.

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub const FINAL_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const TIMEOUT_MSG: &str = "No final transcript within 5 s";

pub trait Ui: Send + Sync {
    fn partial(&self, text: &str);
    fn final_text(&self, text: &str);
    fn error(&self, text: &str);
    fn hide(&self);
}

/// `audio` yields PCM chunks and closes when the key is released. Chunks queue up while the
/// handshake runs (about 1.4 s), so the first words are not lost.
pub async fn run_session(
    audio: mpsc::UnboundedReceiver<Vec<u8>>,
    ui: &dyn Ui,
    key: &str,
    url: &str,
    vocabulary: &[String],
    final_timeout: Duration,
    typer: &std::sync::mpsc::Sender<String>,
) {
    log::info!("session start: {url}"); // never log the key or transcript text
    match stream(audio, ui, key, url, vocabulary, final_timeout, typer).await {
        Ok(()) => {
            ui.hide();
            log::info!("session ok");
        }
        Err(e) => {
            log::warn!("session failed: {e}");
            ui.error(&e);
        }
    }
}

async fn stream(
    mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
    ui: &dyn Ui,
    key: &str,
    url: &str,
    vocabulary: &[String],
    final_timeout: Duration,
    typer: &std::sync::mpsc::Sender<String>,
) -> Result<(), String> {
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("Bad endpoint: {e}"))?;
    let auth = format!("Bearer {key}")
        .parse()
        .map_err(|_| "API key has invalid characters".to_string())?;
    req.headers_mut().insert("Authorization", auth);
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "Connection timed out".to_string())?
        .map_err(|e| format!("Connection failed: {e}"))?;
    let (mut write, mut read) = ws.split();

    if !vocabulary.is_empty() {
        let msg = serde_json::json!({"type": "vocabulary", "phrases": vocabulary}).to_string();
        write
            .send(Message::text(msg))
            .await
            .map_err(|e| e.to_string())?;
    }

    let sender = async {
        let mut bytes = 0usize;
        while let Some(chunk) = audio.recv().await {
            bytes += chunk.len();
            write
                .send(Message::binary(chunk))
                .await
                .map_err(|e| e.to_string())?;
        }
        log::info!("audio sent: {:.1} s", bytes as f64 / 32_000.0); // 16 kHz x 2 bytes
        let done = serde_json::json!({"type": "audio.done"}).to_string();
        write
            .send(Message::text(done))
            .await
            .map_err(|e| e.to_string())
    };

    let receiver = async {
        // Ends at session.ended, or when the server closes the socket.
        while let Some(msg) = read.next().await {
            let Message::Text(raw) = msg.map_err(|e| format!("Connection lost: {e}"))? else {
                continue;
            };
            let msg: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let text = msg["text"].as_str().unwrap_or_default();
            match msg["type"].as_str() {
                Some("transcript.partial") => ui.partial(text),
                Some("transcript.final") if !text.trim().is_empty() => {
                    ui.final_text(text.trim());
                    // Queued, not awaited: slow typing must not count against the final timeout.
                    let _ = typer.send(format!("{} ", text.trim()));
                }
                // The server may then drop TCP without a clean TLS/WebSocket close: stop here.
                Some("session.ended") => return Ok(()),
                Some("error") => {
                    return Err(msg["message"]
                        .as_str()
                        .unwrap_or("unknown API error")
                        .to_string());
                }
                _ => {}
            }
        }
        Ok(())
    };

    tokio::pin!(sender, receiver);
    tokio::select! {
        // The server ended first: an API error or a dropped connection, or a normal close.
        r = &mut receiver => r,
        r = &mut sender => {
            r?;
            tokio::time::timeout(final_timeout, receiver).await.map_err(|_| TIMEOUT_MSG.to_string())?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    #[derive(Default)]
    struct FakeUi(Mutex<Vec<(String, String)>>);
    impl FakeUi {
        fn push(&self, k: &str, t: &str) {
            self.0.lock().unwrap().push((k.into(), t.into()));
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.0.lock().unwrap().clone()
        }
    }
    impl Ui for FakeUi {
        fn partial(&self, t: &str) {
            self.push("partial", t)
        }
        fn final_text(&self, t: &str) {
            self.push("final", t)
        }
        fn error(&self, t: &str) {
            self.push("error", t)
        }
        fn hide(&self) {
            self.push("hide", "")
        }
    }

    #[derive(Default, Debug)]
    struct Got {
        auth: String,
        texts: Vec<serde_json::Value>,
        audio_bytes: usize,
    }

    enum Server {
        Happy,
        Error,
        Silent,
    }

    /// One-connection fake server; returns its ws:// URL and what it received.
    #[allow(clippy::result_large_err)] // the callback's error type is fixed by tungstenite's API
    async fn fake_server(kind: Server) -> (String, Arc<Mutex<Got>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let got = Arc::new(Mutex::new(Got::default()));
        let g = got.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let g2 = g.clone();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, move |req: &Request, resp: Response| {
                    g2.lock().unwrap().auth =
                        req.headers()["Authorization"].to_str().unwrap().into();
                    Ok(resp)
                })
                .await
                .unwrap();
            if let Server::Error = kind {
                let _ = ws
                    .send(Message::text(r#"{"type":"error","message":"bad key"}"#))
                    .await;
            }
            while let Some(Ok(m)) = ws.next().await {
                match m {
                    Message::Binary(b) => g.lock().unwrap().audio_bytes += b.len(),
                    Message::Text(t) => {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                        let done = v["type"] == "audio.done";
                        g.lock().unwrap().texts.push(v);
                        if done && matches!(kind, Server::Happy) {
                            for m in [
                                r#"{"type":"transcript.partial","text":"hello"}"#,
                                r#"{"type":"transcript.final","text":"hello world"}"#,
                                r#"{"type":"transcript.final","text":"  "}"#,
                                r#"{"type":"session.ended"}"#,
                            ] {
                                ws.send(Message::text(m)).await.unwrap();
                            }
                            return; // drop without a close frame, like the real server
                        }
                    }
                    _ => {}
                }
            }
        });
        (url, got)
    }

    /// 1 s of silence, then the "key release" (channel close).
    fn one_second_audio() -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        for _ in 0..10 {
            tx.send(vec![0u8; 3200]).unwrap();
        }
        rx
    }

    #[tokio::test]
    async fn happy_path() {
        let (url, got) = fake_server(Server::Happy).await;
        let ui = FakeUi::default();
        let (typer, typed) = std::sync::mpsc::channel();
        let vocab = vec!["kubectl".to_string()];
        run_session(
            one_second_audio(),
            &ui,
            "test-key",
            &url,
            &vocab,
            FINAL_TIMEOUT,
            &typer,
        )
        .await;
        let got = got.lock().unwrap();
        assert_eq!(got.auth, "Bearer test-key");
        assert_eq!(
            got.texts[0],
            serde_json::json!({"type": "vocabulary", "phrases": ["kubectl"]})
        );
        assert_eq!(
            got.texts.last().unwrap(),
            &serde_json::json!({"type": "audio.done"})
        );
        assert_eq!(got.audio_bytes, 32_000);
        assert_eq!(typed.try_iter().collect::<Vec<_>>(), ["hello world "]);
        let expected: Vec<(String, String)> =
            [("partial", "hello"), ("final", "hello world"), ("hide", "")]
                .map(|(a, b)| (a.into(), b.into()))
                .into();
        assert_eq!(ui.calls(), expected);
    }

    #[tokio::test]
    async fn api_error_does_not_wait_for_audio() {
        let (url, _) = fake_server(Server::Error).await;
        let ui = FakeUi::default();
        let (_tx, rx) = mpsc::unbounded_channel(); // key still held: audio never ends
        tokio::time::timeout(
            Duration::from_secs(5),
            run_session(
                rx,
                &ui,
                "k",
                &url,
                &[],
                FINAL_TIMEOUT,
                &std::sync::mpsc::channel().0,
            ),
        )
        .await
        .expect("error must end the session at once");
        assert_eq!(ui.calls(), [("error".to_string(), "bad key".to_string())]);
    }

    #[tokio::test]
    async fn no_final_times_out() {
        let (url, _) = fake_server(Server::Silent).await;
        let ui = FakeUi::default();
        run_session(
            one_second_audio(),
            &ui,
            "k",
            &url,
            &[],
            Duration::from_millis(300),
            &std::sync::mpsc::channel().0,
        )
        .await;
        assert_eq!(ui.calls(), [("error".to_string(), TIMEOUT_MSG.to_string())]);
    }
}
