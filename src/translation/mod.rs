//! Google 兼容翻译服务：编码、响应契约与 I/O 预算。失败不更换服务。

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use reqwest::{Client as HttpClient, redirect::Policy};
use serde::Deserialize;

use crate::core::{
    DictionaryEntry, Direction, Failure, MAX_SELECTION_BYTES, Request, Service, Translation,
    TranslationFuture, Translator as TranslationPort,
};

pub(crate) fn register() -> fn() -> Result<Arc<dyn TranslationPort>> {
    fn create() -> Result<Arc<dyn TranslationPort>> {
        Ok(Arc::new(Provider::new()?))
    }
    create
}

fn origin(service: Service) -> &'static str {
    match service {
        Service::Mirror => "https://v2g.borber.top",
        Service::Google => "https://translate.googleapis.com",
    }
}

fn target(direction: Direction, text: &str) -> &'static str {
    match direction {
        Direction::Chinese => "zh-CN",
        Direction::English => "en",
        Direction::Japanese => "ja",
        Direction::ChineseEnglish => {
            if text.chars().any(
                |ch| matches!(ch as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0x20000..=0x323af),
            ) {
                "en"
            } else {
                "zh-CN"
            }
        }
    }
}

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const TRANSLATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct GoogleResponse {
    sentences: Option<Vec<Sentence>>,
    dict: Option<Vec<DictionaryEntry>>,
}

#[derive(Deserialize)]
struct Sentence {
    trans: Option<String>,
}

fn parse_translation(bytes: &[u8]) -> std::result::Result<Translation, Failure> {
    // dj=1 契约只接受对象；Serde 的 struct 序列解码不能成为旧数组响应兼容路径。
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(Failure::InvalidResponse);
    }
    let response: GoogleResponse =
        serde_json::from_slice(bytes).map_err(|_| Failure::InvalidResponse)?;
    if response.sentences.is_none() && response.dict.is_none() {
        return Err(Failure::InvalidResponse);
    }
    let dictionary: Vec<_> = response
        .dict
        .into_iter()
        .flatten()
        .filter_map(|mut entry| {
            entry.terms.retain(|term| !term.trim().is_empty());
            (!entry.terms.is_empty()).then_some(entry)
        })
        .collect();
    if !dictionary.is_empty() {
        return Ok(Translation::Dictionary(dictionary));
    }
    let text: String = response
        .sentences
        .into_iter()
        .flatten()
        .filter_map(|part| part.trans)
        .collect();
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Ok(if text.trim().is_empty() {
        Translation::NoResult
    } else {
        Translation::Sentence(text)
    })
}

fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut break_pending = false;
    for ch in text.trim().chars() {
        if matches!(ch, '\r' | '\n') {
            break_pending = true;
        } else {
            if break_pending
                && !out.is_empty()
                && !out.ends_with(char::is_whitespace)
                && !ch.is_whitespace()
            {
                out.push(' ');
            }
            break_pending = false;
            out.push(ch);
        }
    }
    out
}

struct Provider(HttpClient);

impl Provider {
    pub(super) fn new() -> Result<Self> {
        Ok(Self(
            Self::builder().build().context("无法初始化翻译客户端")?,
        ))
    }

    fn builder() -> reqwest::ClientBuilder {
        HttpClient::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(TRANSLATION_TIMEOUT)
            .redirect(Policy::none())
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
    }

    pub(super) async fn translate(
        &self,
        request: &Request,
    ) -> std::result::Result<Translation, Failure> {
        self.send(origin(request.settings.service), request).await
    }

    async fn send(
        &self,
        origin: &str,
        request: &Request,
    ) -> std::result::Result<Translation, Failure> {
        let source = normalize(&request.source);
        if source.len() > MAX_SELECTION_BYTES {
            return Err(Failure::SelectionTooLarge);
        }
        let operation = async {
            let mut response = self
                .0
                .get(format!("{origin}/translate_a/single"))
                .query(&[
                    ("client", "gtx"),
                    ("sl", "auto"),
                    ("tl", target(request.settings.direction, &source)),
                    ("dj", "1"),
                    ("dt", "t"),
                    ("dt", "bd"),
                    ("q", &source),
                ])
                .header("Cache-Control", "no-store")
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(network_failure)?;
            match response.status().as_u16() {
                200..=299 => {}
                429 => return Err(Failure::RateLimited),
                status => return Err(Failure::Http(status)),
            }
            if response
                .content_length()
                .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
            {
                return Err(Failure::ResponseTooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_failure)? {
                if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                    return Err(Failure::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            parse_translation(&bytes)
        };
        tokio::time::timeout(TRANSLATION_TIMEOUT, operation)
            .await
            .map_err(|_| Failure::Timeout)?
    }
}

fn network_failure(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else {
        Failure::Network
    }
}

impl TranslationPort for Provider {
    fn translate<'a>(&'a self, request: &'a Request) -> TranslationFuture<'a> {
        Box::pin(Provider::translate(self, request))
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{Settings, Ticket};

    #[test]
    fn automatic_direction_is_limited_to_chinese_english() {
        assert_eq!(target(Direction::ChineseEnglish, "你好"), "en");
        assert_eq!(target(Direction::ChineseEnglish, "hello"), "zh-CN");
        assert_eq!(target(Direction::ChineseEnglish, "日本"), "en");
        assert_eq!(target(Direction::Japanese, "日本"), "ja");
        assert_eq!(target(Direction::Chinese, "hello"), "zh-CN");
        assert_eq!(target(Direction::English, "你好"), "en");
    }
    use super::*;
    use std::sync::Arc;

    // 固定到回环地址；不让测试继承代理并意外访问外网。其余请求策略与运行客户端同源。
    fn local_translator(timeout: Duration) -> Provider {
        Provider(
            Provider::builder()
                .no_proxy()
                .timeout(timeout)
                .build()
                .unwrap(),
        )
    }

    #[test]
    fn parser_keeps_lf_crlf_and_multiple_sentences() {
        let result =
            parse_translation(br#"{"sentences":[{"trans":"a\nb\r\nc\rd"},{"trans":" end"}]}"#)
                .unwrap();
        assert_eq!(result, Translation::Sentence("a\nb\nc\nd end".into()));
    }

    #[test]
    fn response_shape_errors_are_not_empty_results() {
        for bytes in [
            b"{}".as_slice(),
            b"[]",
            b"[[]]",
            br#"[[{"trans":"not an object response"}],null]"#,
            br#"{"sentences":{}}"#,
            br#"{"sentences":[{"trans":42}]}"#,
        ] {
            assert_eq!(parse_translation(bytes), Err(Failure::InvalidResponse));
        }
        assert_eq!(
            parse_translation(br#"{"sentences":[]}"#),
            Ok(Translation::NoResult)
        );
    }

    #[test]
    fn dictionary_contains_only_usable_terms() {
        let result = parse_translation(
            br#"{"dict":[{"pos":"noun","terms":[" ","meaning"]}],"sentences":[]}"#,
        )
        .unwrap();
        assert_eq!(
            result,
            Translation::Dictionary(vec![DictionaryEntry {
                pos: "noun".into(),
                terms: vec!["meaning".into()]
            }])
        );
    }

    #[test]
    fn normalization_preserves_word_boundaries() {
        assert_eq!(normalize(" \na\r\nb\n\n c\n "), "a b c");
        assert_eq!(normalize("a\n b"), "a b");
        assert_eq!(normalize("\n\r"), "");
    }

    async fn response_server(response: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let _ = stream.write_all(&response).await;
        });
        (origin, server)
    }

    #[tokio::test]
    async fn response_limits_apply_with_and_without_content_length_and_redirects_are_rejected() {
        let request = Request {
            ticket: Ticket(1),
            source: Arc::from("hello"),
            settings: Settings::default(),
        };
        let oversized_header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let mut oversized_body = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        oversized_body.extend(vec![b'a'; MAX_RESPONSE_BYTES + 1]);
        for (response, expected) in [
            (b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/other\r\nContent-Length: 0\r\n\r\n".to_vec(), Failure::Http(302)),
            (oversized_header, Failure::ResponseTooLarge),
            (oversized_body, Failure::ResponseTooLarge),
        ] {
            let (origin, server) = response_server(response).await;
            assert_eq!(local_translator(TRANSLATION_TIMEOUT).send(&origin, &request).await, Err(expected));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_slow_response_body_is_bounded_and_does_not_expose_source_text() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let translator = local_translator(Duration::from_millis(80));
        let request = Request {
            ticket: Ticket(1),
            source: Arc::from("private sample"),
            settings: Settings::default(),
        };
        let started = std::time::Instant::now();
        let error = translator.send(&origin, &request).await.unwrap_err();
        assert_eq!(error, Failure::Timeout);
        assert!(!error.to_string().contains("private"));
        assert!(started.elapsed() < Duration::from_secs(1));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn request_encoding_error_privacy_and_rate_limit_use_local_server() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let len = stream.read(&mut bytes).await.unwrap();
            let request = String::from_utf8(bytes[..len].to_vec()).unwrap();
            assert!(request.contains("dt=t&dt=bd"));
            assert!(request.contains("q=private+%26+text"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("cache-control: no-store\r\n")
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("accept: application/json\r\n")
            );
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let request = Request {
            ticket: Ticket(1),
            source: Arc::from("private & text"),
            settings: Settings::default(),
        };
        let failure = local_translator(TRANSLATION_TIMEOUT)
            .send(&origin, &request)
            .await
            .unwrap_err();
        assert_eq!(failure, Failure::RateLimited);
        assert!(!failure.to_string().contains("private"));
        server.await.unwrap();
    }
}
