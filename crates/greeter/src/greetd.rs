//! greetd's IPC, over `$GREETD_SOCK`: requests and responses through greetd_ipc, except
//! the answer to a secret prompt, whose frame is built by hand in a locked buffer and wiped
//! once written: greetd_ipc would serialise it from a `String` into a `Vec`, two copies left
//! unwiped on the heap.

use std::env;

use anyhow::{Context, anyhow, ensure};
use greetd_ipc::codec::TokioCodec;
use greetd_ipc::{Request, Response};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use ui::password::{self, Locked};
use zeroize::Zeroize;

/// A frame is its body's length (4 bytes, native byte order), then the body: this, the
/// answer as a JSON string's contents, and `SUFFIX`.
const PREFIX: &[u8] = br#"{"type":"post_auth_message_response","response":""#;
const SUFFIX: &[u8] = br#""}"#;

/// The frame's buffer: two pages, enough for a password of `password::CAPACITY` bytes even
/// if each were escaped as `\u00XX`.
const FRAME_CAPACITY: usize = 8192;
const _: () = assert!(4 + PREFIX.len() + 6 * password::CAPACITY + SUFFIX.len() <= FRAME_CAPACITY);

pub struct Greetd {
    writer: OwnedWriteHalf,
    /// Read by a task of their own, so that waiting for one can be cancelled.
    responses: mpsc::Receiver<anyhow::Result<Response>>,
    /// Where the secret answer's frame is built.
    frame: Locked<FRAME_CAPACITY>,
}

impl Greetd {
    pub async fn connect() -> anyhow::Result<Self> {
        let path = env::var_os("GREETD_SOCK").context("GREETD_SOCK is not set")?;
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting to greetd at {}", path.display()))?;
        Ok(Self::new(stream))
    }

    /// greetd at the other end of `stream`; spawns the task that reads its responses.
    pub fn new(stream: UnixStream) -> Self {
        let (mut reader, writer) = stream.into_split();
        let (sender, responses) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let response = Response::read_from(&mut reader).await;
                let failed = response.is_err();
                let response = response.map_err(|e| anyhow!(e).context("reading from greetd"));
                if sender.send(response).await.is_err() || failed {
                    return;
                }
            }
        });
        Self {
            writer,
            responses,
            frame: Locked::new(),
        }
    }

    pub async fn request(&mut self, request: &Request) -> anyhow::Result<()> {
        request
            .write_to(&mut self.writer)
            .await
            .context("writing to greetd")
    }

    /// Answer greetd's prompt with `answer`, UTF-8; for a secret prompt, in a frame built in
    /// the locked buffer and wiped once written.
    pub async fn answer(&mut self, answer: &[u8], secret: bool) -> anyhow::Result<()> {
        if !secret {
            let answer = String::from_utf8(answer.to_vec()).context("the answer is not UTF-8")?;
            let response = Some(answer);
            return self
                .request(&Request::PostAuthMessageResponse { response })
                .await;
        }
        let written = match secret_frame(answer, &mut self.frame) {
            Ok(len) => self
                .writer
                .write_all(&self.frame[..len])
                .await
                .context("writing to greetd"),
            Err(e) => Err(e),
        };
        self.frame.zeroize();
        written
    }

    /// greetd's next response. Cancel-safe.
    pub async fn receive(&mut self) -> anyhow::Result<Response> {
        let response = self.responses.recv().await;
        response.unwrap_or_else(|| Err(anyhow!("greetd closed the connection")))
    }
}

/// Build in `frame` the frame of a `post_auth_message_response` request with `answer` (UTF-8)
/// as its response, escaped for JSON; returns its length.
fn secret_frame(answer: &[u8], frame: &mut [u8; FRAME_CAPACITY]) -> anyhow::Result<usize> {
    ensure!(answer.len() <= password::CAPACITY, "the answer is too long");
    let mut len = 4;
    let mut put = |bytes: &[u8]| {
        frame[len..len + bytes.len()].copy_from_slice(bytes);
        len += bytes.len();
    };
    put(PREFIX);
    for &byte in answer {
        match byte {
            b'"' => put(br#"\""#),
            b'\\' => put(br"\\"),
            // Control characters as `\u00XX`; UTF-8's other bytes as they are.
            0..0x20 => {
                let hex = |digit: u8| b"0123456789abcdef"[usize::from(digit)];
                put(&[b'\\', b'u', b'0', b'0', hex(byte >> 4), hex(byte & 0xf)]);
            }
            _ => put(&[byte]),
        }
    }
    put(SUFFIX);
    let body = u32::try_from(len - 4).expect("FRAME_CAPACITY fits a u32");
    frame[..4].copy_from_slice(&body.to_ne_bytes());
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request greetd_ipc decodes from `frame`.
    async fn decode(mut frame: &[u8]) -> Request {
        let request = Request::read_from(&mut frame).await.unwrap();
        assert!(frame.is_empty(), "bytes left over");
        request
    }

    #[tokio::test]
    async fn the_secret_frame_decodes_to_the_same_answer() {
        let answers = [
            "correct horse".to_owned(),
            r#"quo"te and back\slash \" \\"#.to_owned(),
            "tab\tnewline\ncr\rnul\0bell\u{7}esc\u{1b}del\u{7f}".to_owned(),
            "é€😀 ünïcödé \u{2028}".to_owned(),
            String::new(),
            "\\".repeat(password::CAPACITY),
            "\u{1}".repeat(password::CAPACITY),
        ];
        let mut frame = [0; FRAME_CAPACITY];
        for answer in answers {
            let len = secret_frame(answer.as_bytes(), &mut frame).unwrap();
            let want = Some(answer.clone());
            match decode(&frame[..len]).await {
                Request::PostAuthMessageResponse { response } => assert!(response == want),
                _ => panic!("not an answer: {answer:?}"),
            }
        }
    }

    #[test]
    fn an_answer_longer_than_a_password_is_refused() {
        let mut frame = [0; FRAME_CAPACITY];
        let answer = vec![b'a'; password::CAPACITY + 1];
        assert!(secret_frame(&answer, &mut frame).is_err());
    }

    #[tokio::test]
    async fn answers_and_requests_reach_greetd_and_the_frame_is_wiped() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut greetd = Greetd::new(ours);
        greetd.answer("s3cr\"t".as_bytes(), true).await.unwrap();
        assert!(greetd.frame.iter().all(|&b| b == 0));
        greetd.answer(b"visible", false).await.unwrap();
        let request = Request::CreateSession {
            username: "alice".into(),
        };
        greetd.request(&request).await.unwrap();
        for want in [Some("s3cr\"t"), Some("visible")] {
            match Request::read_from(&mut theirs).await.unwrap() {
                Request::PostAuthMessageResponse { response } => {
                    assert_eq!(response.as_deref(), want)
                }
                _ => panic!("not an answer"),
            }
        }
        let request = Request::read_from(&mut theirs).await.unwrap();
        assert!(matches!(request, Request::CreateSession { username } if username == "alice"));
        Response::Success.write_to(&mut theirs).await.unwrap();
        assert!(matches!(greetd.receive().await.unwrap(), Response::Success));
        drop(theirs);
        assert!(greetd.receive().await.is_err());
    }
}
