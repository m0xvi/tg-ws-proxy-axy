use crate::config::*;
use crate::crypto::xor_mask_in_place;
use crate::{ldebug};
use base64::Engine;
use byteorder::{BigEndian, ByteOrder};
use rand::RngCore;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use std::collections::HashMap;
use std::io::IoSlice;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

// ---------------------------------------------------------------------------
// DPI Bypass Stream — TLS ClientHello Record Splitting & TCP Segmentation
// ---------------------------------------------------------------------------

/// Разбивает один TLS ClientHello рекорд (0x16 0x03 ..) на 3 валидных TLS рекорда,
/// причём второй разрез приходится прямо на середину SNI-домена.
/// Это позволяет обходить пассивный и активный DPI (ТСПУ) на мобильных сетях без root/VPN.
fn split_tls_client_hello_chunks(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    if buf.len() < 16 || buf[0] != 0x16 || buf[1] != 0x03 {
        return None;
    }
    let rec_len = ((buf[3] as usize) << 8) | (buf[4] as usize);
    if 5 + rec_len > buf.len() || rec_len < 10 {
        return None;
    }
    let ver_major = buf[1];
    let ver_minor = buf[2];
    let payload = &buf[5..5 + rec_len];
    // Handshake type 0x01 == ClientHello
    if payload[0] != 0x01 {
        return None;
    }

    // Ищем позицию SNI внутри ClientHello (после первых 38 байт заголовка/Random),
    // чтобы разрезать пакет ровно посреди имени хоста.
    let search_start = 38.min(payload.len());
    let mut best_run_start = 0usize;
    let mut best_run_len = 0usize;
    let mut cur_start = search_start;
    let mut cur_len = 0usize;
    let mut has_dot = false;

    for (idx, &b) in payload[search_start..].iter().enumerate() {
        let abs_idx = search_start + idx;
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
            if cur_len == 0 {
                cur_start = abs_idx;
                has_dot = false;
            }
            if b == b'.' {
                has_dot = true;
            }
            cur_len += 1;
            if has_dot && cur_len > best_run_len {
                best_run_len = cur_len;
                best_run_start = cur_start;
            }
        } else {
            cur_len = 0;
            has_dot = false;
        }
    }

    let split1 = 2usize; // сразу после Handshake Type (0x01) и старшего байта длины
    let split2 = if best_run_len >= 6 {
        (best_run_start + best_run_len / 2).clamp(split1 + 2, payload.len() - 2)
    } else {
        (payload.len() / 2).clamp(split1 + 2, payload.len() - 2)
    };

    let part1 = &payload[..split1];
    let part2 = &payload[split1..split2];
    let part3 = &payload[split2..];

    let make_record = |slice: &[u8]| -> Vec<u8> {
        let l = slice.len();
        let mut rec = Vec::with_capacity(5 + l);
        rec.push(0x16);
        rec.push(ver_major);
        rec.push(ver_minor);
        rec.push(((l >> 8) & 0xFF) as u8);
        rec.push((l & 0xFF) as u8);
        rec.extend_from_slice(slice);
        rec
    };

    let mut chunks = Vec::with_capacity(4);
    chunks.push(make_record(part1));
    chunks.push(make_record(part2));
    chunks.push(make_record(part3));
    if 5 + rec_len < buf.len() {
        chunks.push(buf[5 + rec_len..].to_vec());
    }
    Some(chunks)
}

pub struct DpiBypassStream {
    inner: TcpStream,
    first_write_done: bool,
    pending_chunks: Vec<Vec<u8>>,
    chunk_idx: usize,
    byte_idx: usize,
    orig_len: usize,
}

impl DpiBypassStream {
    pub fn new(inner: TcpStream) -> Self {
        Self {
            inner,
            first_write_done: false,
            pending_chunks: Vec::new(),
            chunk_idx: 0,
            byte_idx: 0,
            orig_len: 0,
        }
    }
}

impl AsyncRead for DpiBypassStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DpiBypassStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.first_write_done {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        }

        if self.pending_chunks.is_empty() {
            if !DPI_FRAGMENTATION.load(Ordering::Relaxed) {
                self.first_write_done = true;
                return Pin::new(&mut self.inner).poll_write(cx, buf);
            }
            if let Some(chunks) = split_tls_client_hello_chunks(buf) {
                self.pending_chunks = chunks;
                self.chunk_idx = 0;
                self.byte_idx = 0;
                self.orig_len = buf.len();
            } else {
                self.first_write_done = true;
                return Pin::new(&mut self.inner).poll_write(cx, buf);
            }
        }

        while self.chunk_idx < self.pending_chunks.len() {
            let chunk_len = self.pending_chunks[self.chunk_idx].len();
            let byte_idx = self.byte_idx;
            let slice = self.pending_chunks[self.chunk_idx][byte_idx..].to_vec();
            match Pin::new(&mut self.inner).poll_write(cx, &slice) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write zero during TLS DPI fragmentation",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    self.byte_idx += n;
                    if self.byte_idx >= chunk_len {
                        self.chunk_idx += 1;
                        self.byte_idx = 0;
                        let _ = Pin::new(&mut self.inner).poll_flush(cx);
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        let total = self.orig_len;
        self.pending_chunks.clear();
        self.first_write_done = true;
        Poll::Ready(Ok(total))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        if !self.first_write_done {
            let mut flat = Vec::new();
            for b in bufs {
                flat.extend_from_slice(b);
            }
            return self.poll_write(cx, &flat);
        }
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.first_write_done && self.inner.is_write_vectored()
    }
}

// ---------------------------------------------------------------------------
// WS opcodes — proxy/raw_websocket.py
// ---------------------------------------------------------------------------

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

pub const MAX_MESSAGE_LEN: u64 = 16 * 1024 * 1024;
const MAX_FRAME_PAYLOAD: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// TLS config: InsecureSkipVerify + session cache (как в Go)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

use once_cell::sync::Lazy;

// Глобальный TLS-конфиг с session resumption cache (аналог tls.NewLRUClientSessionCache(100))
static TLS_CONFIG: Lazy<Arc<ClientConfig>> = Lazy::new(|| {
    let mut cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    cfg.resumption = rustls::client::Resumption::in_memory_sessions(100);
    Arc::new(cfg)
});

// Отдельный контекст для fronting (check_hostname=False в оригинале).
// На практике оба контекста NoVerify; разделение сохранено для паритета.
static TLS_CONFIG_FRONTING: Lazy<Arc<ClientConfig>> = Lazy::new(|| TLS_CONFIG.clone());

// ---------------------------------------------------------------------------
// WsHandshakeError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WsHandshakeError {
    pub status_code: i32,
    pub status_line: String,
    pub headers: HashMap<String, String>,
    pub location: String,
}

impl WsHandshakeError {
    pub fn is_redirect(&self) -> bool {
        matches!(self.status_code, 301 | 302 | 303 | 307 | 308)
    }
}

#[derive(Debug)]
pub enum WsError {
    Io(std::io::Error),
    Handshake(WsHandshakeError),
    Timeout,
    Canceled,
    Other(String),
}

impl WsError {
    pub fn compact(&self) -> String {
        match self {
            WsError::Canceled => "canceled".to_string(),
            WsError::Timeout => "timeout".to_string(),
            WsError::Handshake(h) => format!("http {}", h.status_code),
            WsError::Io(e) => {
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock
                {
                    "timeout".to_string()
                } else {
                    e.to_string()
                }
            }
            WsError::Other(s) => s.clone(),
        }
    }
    pub fn handshake_status(&self) -> Option<i32> {
        if let WsError::Handshake(h) = self {
            Some(h.status_code)
        } else {
            None
        }
    }
    pub fn handshake(&self) -> Option<&WsHandshakeError> {
        if let WsError::Handshake(h) = self {
            Some(h)
        } else {
            None
        }
    }
}

pub fn is_http_status_error(err: &WsError, code: i32) -> bool {
    err.handshake_status() == Some(code)
}

impl From<std::io::Error> for WsError {
    fn from(e: std::io::Error) -> Self {
        WsError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// RawWebSocket — порт proxy/raw_websocket.py
// Поддерживает TLS (443) и plain (80, disable_secure), SNI-fronting,
// фрагментацию (CONT/TEXT/BINARY reassembly), close reasons.
// ---------------------------------------------------------------------------

enum WsReader {
    Tls(BufReader<tokio::io::ReadHalf<TlsStream<DpiBypassStream>>>),
    Plain(BufReader<tokio::io::ReadHalf<TcpStream>>),
}

enum WsWriter {
    Tls(tokio::io::WriteHalf<TlsStream<DpiBypassStream>>),
    Plain(tokio::io::WriteHalf<TcpStream>),
}

pub struct RawWebSocket {
    reader: tokio::sync::Mutex<WsReader>,
    writer: tokio::sync::Mutex<WsWriter>,
    frag: tokio::sync::Mutex<Vec<u8>>,
    pub closed: AtomicBool,
    pub secure: bool,
    pub remote_addr: String,
    pub remote_port: u16,
}

impl RawWebSocket {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Совместимость: закрыт ли транспорт (аналог writer.transport.is_closing()).
    pub fn transport_closing(&self) -> bool {
        self.is_closed()
    }

    pub async fn send(&self, data: &[u8]) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        let frame = build_frame(OP_BINARY, data, true);
        self.write_frame(&frame, WS_WRITE_TIMEOUT).await
    }

    pub async fn send_batch(&self, parts: &[Vec<u8>]) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        let mut writer = self.writer.lock().await;
        for part in parts {
            let frame = build_frame(OP_BINARY, part, true);
            let res = tokio::time::timeout(WS_WRITE_TIMEOUT, write_all_match(&mut writer, &frame)).await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(WsError::Io(e));
                }
                Err(_) => {
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(WsError::Timeout);
                }
            }
        }
        Ok(())
    }

    pub async fn send_ping(&self) -> Result<(), WsError> {
        if self.is_closed() {
            return Err(WsError::Other("WebSocket closed".to_string()));
        }
        let frame = build_frame(OP_PING, &[], true);
        self.write_frame(&frame, WS_CONTROL_TIMEOUT).await
    }

    async fn write_frame(&self, frame: &[u8], timeout: Duration) -> Result<(), WsError> {
        let mut writer = self.writer.lock().await;
        let res = if timeout > Duration::ZERO {
            tokio::time::timeout(timeout, write_all_match(&mut writer, frame)).await
        } else {
            Ok(write_all_match(&mut writer, frame).await)
        };
        match res {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.closed.store(true, Ordering::Relaxed);
                Err(WsError::Io(e))
            }
            Err(_) => {
                self.closed.store(true, Ordering::Relaxed);
                Err(WsError::Timeout)
            }
        }
    }

    // Recv с реассемблированием фрагментов (как Go/Python Recv).
    pub async fn recv(&self) -> Result<Vec<u8>, WsError> {
        loop {
            if self.is_closed() {
                return Err(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF",
                )));
            }
            let (opcode, payload, fin) = match self.read_frame().await {
                Ok(v) => v,
                Err(e) => {
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            };
            match opcode {
                OP_CLOSE => {
                    self.closed.store(true, Ordering::Relaxed);
                    let (code, reason) = parse_close(&payload);
                    ldebug!("WS OP_CLOSE from upstream: code={:?} reason={:?}", code, reason);
                    let mut close_payload = payload;
                    if close_payload.len() > 2 {
                        close_payload.truncate(2);
                    }
                    let reply = build_frame(OP_CLOSE, &close_payload, true);
                    let _ = self.write_frame(&reply, WS_CONTROL_TIMEOUT).await;
                    return Err(WsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF",
                    )));
                }
                OP_PING => {
                    let pong = build_frame(OP_PONG, &payload, true);
                    let _ = self.write_frame(&pong, WS_CONTROL_TIMEOUT).await;
                    continue;
                }
                OP_PONG => continue,
                OP_CONT | OP_TEXT | OP_BINARY => {
                    let mut frag = self.frag.lock().await;
                    if fin && frag.is_empty() {
                        return Ok(payload);
                    }
                    if frag.len() + payload.len() > MAX_MESSAGE_LEN as usize {
                        frag.clear();
                        return Err(WsError::Other(format!(
                            "WS message too large: {} bytes",
                            frag.len() + payload.len()
                        )));
                    }
                    frag.extend_from_slice(&payload);
                    if !fin {
                        continue;
                    }
                    let msg = std::mem::take(&mut *frag);
                    return Ok(msg);
                }
                _ => continue,
            }
        }
    }

    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        // Best-effort close frame + shutdown транспорта.
        let frame = build_frame(OP_CLOSE, &[], true);
        let _ = self.write_frame(&frame, WS_CONTROL_TIMEOUT).await;
        let mut writer = self.writer.lock().await;
        let _ = shutdown_match(&mut writer).await;
    }

    // recv с дедлайном чтения (для bridge).
    // ВАЖНО: таймаут оборачивает только чтение ОДНОГО фрейма целиком под
    // удержанием lock, чтобы НЕ дропать future посреди read_exact (иначе
    // теряются уже прочитанные байты BufReader → рассинхрон потока).
    pub async fn recv_with_timeout(&self, dur: Duration) -> Result<Vec<u8>, WsError> {
        loop {
            if self.is_closed() {
                return Err(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF",
                )));
            }
            let frame = {
                let mut reader = self.reader.lock().await;
                match tokio::time::timeout(dur, read_frame_match(&mut reader)).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        self.closed.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                    Err(_) => return Err(WsError::Timeout),
                }
            };
            let (opcode, payload, fin) = frame;
            match opcode {
                OP_CLOSE => {
                    self.closed.store(true, Ordering::Relaxed);
                    let mut close_payload = payload;
                    if close_payload.len() > 2 {
                        close_payload.truncate(2);
                    }
                    let reply = build_frame(OP_CLOSE, &close_payload, true);
                    let _ = self.write_frame(&reply, WS_CONTROL_TIMEOUT).await;
                    return Err(WsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF",
                    )));
                }
                OP_PING => {
                    let pong = build_frame(OP_PONG, &payload, true);
                    let _ = self.write_frame(&pong, WS_CONTROL_TIMEOUT).await;
                    continue;
                }
                OP_PONG => continue,
                OP_CONT | OP_TEXT | OP_BINARY => {
                    let mut frag = self.frag.lock().await;
                    if fin && frag.is_empty() {
                        return Ok(payload);
                    }
                    if frag.len() + payload.len() > MAX_MESSAGE_LEN as usize {
                        frag.clear();
                        return Err(WsError::Other("WS message too large".to_string()));
                    }
                    frag.extend_from_slice(&payload);
                    if !fin {
                        continue;
                    }
                    let msg = std::mem::take(&mut *frag);
                    return Ok(msg);
                }
                _ => continue,
            }
        }
    }

    async fn read_frame(&self) -> Result<(u8, Vec<u8>, bool), WsError> {
        let mut reader = self.reader.lock().await;
        read_frame_match(&mut reader).await
    }
}

async fn write_all_match(writer: &mut WsWriter, frame: &[u8]) -> std::io::Result<()> {
    match writer {
        WsWriter::Tls(w) => w.write_all(frame).await,
        WsWriter::Plain(w) => w.write_all(frame).await,
    }
}

async fn shutdown_match(writer: &mut WsWriter) -> std::io::Result<()> {
    match writer {
        WsWriter::Tls(w) => w.shutdown().await,
        WsWriter::Plain(w) => w.shutdown().await,
    }
}

async fn read_frame_match(reader: &mut WsReader) -> Result<(u8, Vec<u8>, bool), WsError> {
    match reader {
        WsReader::Tls(r) => read_frame_generic(r).await,
        WsReader::Plain(r) => read_frame_generic(r).await,
    }
}

// Чтение одного фрейма (generic над TLS/plain reader).
async fn read_frame_generic<R>(reader: &mut BufReader<R>) -> Result<(u8, Vec<u8>, bool), WsError>
where
    R: AsyncReadExt + Unpin,
{
    let mut hdr = [0u8; 2];
    reader.read_exact(&mut hdr).await?;

    let fin = (hdr[0] & 0x80) != 0;
    let opcode = hdr[0] & 0x0F;
    let mut length = (hdr[1] & 0x7F) as u64;

    if length == 126 {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).await?;
        length = BigEndian::read_u16(&buf) as u64;
    } else if length == 127 {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).await?;
        length = BigEndian::read_u64(&buf);
    }

    let has_mask = (hdr[1] & 0x80) != 0;
    let mut mask_key = [0u8; 4];
    if has_mask {
        reader.read_exact(&mut mask_key).await?;
    }

    if length > MAX_FRAME_PAYLOAD {
        return Err(WsError::Other(format!("frame too large: {} bytes", length)));
    }
    let mut payload = vec![0u8; length as usize];
    if length > 0 {
        reader.read_exact(&mut payload).await?;
    }
    if has_mask {
        xor_mask_in_place(&mut payload, &mask_key);
    }
    Ok((opcode, payload, fin))
}

fn parse_close(payload: &[u8]) -> (Option<u16>, String) {
    if payload.len() < 2 {
        return (None, String::new());
    }
    let code = BigEndian::read_u16(&payload[..2]);
    let text = String::from_utf8_lossy(&payload[2..]).to_string();
    let name = match code {
        1000 => "normal",
        1001 => "going_away",
        1002 => "protocol_error",
        1003 => "unsupported_data",
        1006 => "abnormal",
        1007 => "bad_data",
        1008 => "policy_violation",
        1009 => "too_big",
        1010 => "missing_extension",
        1011 => "internal_error",
        _ => "",
    };
    if name.is_empty() {
        (Some(code), text)
    } else if text.is_empty() {
        (Some(code), name.to_string())
    } else {
        (Some(code), format!("{} ({})", text, name))
    }
}

// ---------------------------------------------------------------------------
// Frame builder (mask=true всегда для клиента)
// ---------------------------------------------------------------------------

pub fn build_frame(opcode: u8, data: &[u8], mask: bool) -> Vec<u8> {
    let length = data.len();
    let fb = 0x80 | opcode;

    let mut header_size = 2;
    if mask {
        header_size += 4;
    }
    if length >= 126 && length < 65536 {
        header_size += 2;
    } else if length >= 65536 {
        header_size += 8;
    }

    let total_size = header_size + length;
    let mut result = vec![0u8; total_size];

    let mut pos = 0;
    result[pos] = fb;
    pos += 1;

    let mut mask_key = [0u8; 4];
    if mask {
        rand::thread_rng().fill_bytes(&mut mask_key);
    }

    if length < 126 {
        let mut lb = length as u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
    } else if length < 65536 {
        let mut lb = 126u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
        BigEndian::write_u16(&mut result[pos..], length as u16);
        pos += 2;
    } else {
        let mut lb = 127u8;
        if mask {
            lb |= 0x80;
        }
        result[pos] = lb;
        pos += 1;
        BigEndian::write_u64(&mut result[pos..], length as u64);
        pos += 8;
    }

    if mask {
        result[pos..pos + 4].copy_from_slice(&mask_key);
        pos += 4;
        result[pos..pos + length].copy_from_slice(data);
        xor_mask_in_place(&mut result[pos..pos + length], &mask_key);
    } else {
        result[pos..pos + length].copy_from_slice(data);
    }
    result
}

// ---------------------------------------------------------------------------
// Connection helpers — порт RawWebSocket.connect(host, domain, timeout,
// path, sni, secure) из proxy/raw_websocket.py
// ---------------------------------------------------------------------------

fn set_sock_opts(stream: &TcpStream) {
    if TCP_NODELAY {
        let _ = stream.set_nodelay(true);
    }
    // Аналог Go/Python: keepalive 30s — детект мёртвых соединений на мобиле.
    let sock = socket2::SockRef::from(stream);
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30));
    let _ = sock.set_tcp_keepalive(&ka);
    // proxy/raw_websocket.py::set_sock_opts — buffer_size из конфига.
    let buf = BUFFER_SIZE.load(std::sync::atomic::Ordering::Relaxed);
    if buf > 0 {
        let b = buf as usize;
        let _ = sock.set_recv_buffer_size(b);
        let _ = sock.set_send_buffer_size(b);
    }
}

pub fn ws_connect_timeout(timeout: f64) -> Duration {
    if timeout <= 0.0 {
        Duration::from_secs(5)
    } else {
        Duration::from_secs_f64(timeout)
    }
}

pub fn ws_handshake_timeout(total: Duration) -> Duration {
    if total <= Duration::ZERO {
        Duration::from_secs(3)
    } else if total > Duration::from_secs(3) {
        Duration::from_secs(3)
    } else {
        total
    }
}

fn server_name(domain: &str) -> ServerName<'static> {
    ServerName::try_from(domain.to_string())
        .unwrap_or_else(|_| ServerName::IpAddress("127.0.0.1".parse::<IpAddr>().unwrap().into()))
}

// Типизированный handshake: возвращает готовый RawWebSocket без type-erasure.
async fn handshake_tls(
    read_half: tokio::io::ReadHalf<TlsStream<DpiBypassStream>>,
    mut write_half: tokio::io::WriteHalf<TlsStream<DpiBypassStream>>,
    domain: &str,
    path: &str,
    timeout: Duration,
) -> Result<RawWebSocket, WsError> {
    let mut ws_key_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut ws_key_bytes);
    let ws_key = base64::engine::general_purpose::STANDARD.encode(ws_key_bytes);
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n",
        path, domain, ws_key
    );
    match tokio::time::timeout(timeout, write_half.write_all(req.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(WsError::Io(e)),
        Err(_) => return Err(WsError::Timeout),
    }
    let mut bufreader = BufReader::with_capacity(4096, read_half);
    let (status_code, first_line, headers) =
        read_http_response(&mut bufreader, timeout).await?;
    if status_code == 101 {
        return Ok(RawWebSocket {
            reader: tokio::sync::Mutex::new(WsReader::Tls(bufreader)),
            writer: tokio::sync::Mutex::new(WsWriter::Tls(write_half)),
            frag: tokio::sync::Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            secure: true,
            remote_addr: String::new(),
            remote_port: 443,
        });
    }
    let location = headers.get("location").cloned().unwrap_or_default();
    Err(WsError::Handshake(WsHandshakeError {
        status_code,
        status_line: first_line,
        headers,
        location,
    }))
}

async fn handshake_plain(
    read_half: tokio::io::ReadHalf<TcpStream>,
    mut write_half: tokio::io::WriteHalf<TcpStream>,
    domain: &str,
    path: &str,
    timeout: Duration,
) -> Result<RawWebSocket, WsError> {
    let mut ws_key_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut ws_key_bytes);
    let ws_key = base64::engine::general_purpose::STANDARD.encode(ws_key_bytes);

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n",
        path, domain, ws_key
    );
    let write_res = tokio::time::timeout(timeout, write_half.write_all(req.as_bytes())).await;

    match write_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(WsError::Io(e)),
        Err(_) => return Err(WsError::Timeout),
    }
    let mut bufreader = BufReader::with_capacity(4096, read_half);
    let (status_code, first_line, headers) =
        read_http_response(&mut bufreader, timeout).await?;
    if status_code == 101 {
        return Ok(RawWebSocket {
            reader: tokio::sync::Mutex::new(WsReader::Plain(bufreader)),
            writer: tokio::sync::Mutex::new(WsWriter::Plain(write_half)),
            frag: tokio::sync::Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            secure: false,
            remote_addr: String::new(),
            remote_port: 80,
        });
    }
    let location = headers.get("location").cloned().unwrap_or_default();
    Err(WsError::Handshake(WsHandshakeError {
        status_code,
        status_line: first_line,
        headers,
        location,
    }))
}

async fn read_http_response<R: AsyncReadExt + Unpin>(
    bufreader: &mut BufReader<R>,
    timeout: Duration,
) -> Result<(i32, String, HashMap<String, String>), WsError> {
    let mut response_lines: Vec<String> = Vec::new();
    let read_result = tokio::time::timeout(timeout, async {
        loop {
            let line = read_line(bufreader).await?;
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            response_lines.push(line);
            if response_lines.len() > 100 {
                return Err(WsError::Other("too many HTTP headers".to_string()));
            }
        }
        Ok::<(), WsError>(())
    })
    .await;
    match read_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(WsError::Timeout),
    }
    if response_lines.is_empty() {
        return Err(WsError::Handshake(WsHandshakeError {
            status_code: 0,
            status_line: "empty response".to_string(),
            headers: HashMap::new(),
            location: String::new(),
        }));
    }
    let first_line = response_lines[0].clone();
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    let mut status_code = 0;
    if parts.len() >= 2 {
        status_code = parts[1].parse::<i32>().unwrap_or(0);
    }
    let mut headers = HashMap::new();
    for hl in &response_lines[1..] {
        if let Some(idx) = hl.find(':') {
            headers.insert(
                hl[..idx].trim().to_lowercase(),
                hl[idx + 1..].trim().to_string(),
            );
        }
    }
    Ok((status_code, first_line, headers))
}

async fn read_line<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<String, WsError> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(WsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF",
            )));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() > 16384 {
            return Err(WsError::Other("header line too long".to_string()));
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Форматирует адрес и порт с поддержкой IPv6 (`[2606:4700::1]:443`).
pub fn format_host_port(dial_addr: &str, port: u16) -> String {
    let trimmed = dial_addr.trim();
    if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{}]:{}", trimmed, port)
    } else {
        format!("{}:{}", trimmed, port)
    }
}

// wsConnectOnce — заголовки 1-в-1 как в Python raw_websocket.py (без User-Agent).
pub async fn ws_connect_once(
    dial_addr: &str,
    domain: &str,
    path: &str,
    timeout: Duration,
) -> Result<RawWebSocket, WsError> {
    let secure = !DISABLE_SECURE.load(Ordering::Relaxed);
    ws_connect_once_full(dial_addr, domain, path, timeout, None, secure).await
}

pub async fn ws_connect_once_full(
    dial_addr: &str,
    domain: &str,
    path: &str,
    timeout: Duration,
    sni: Option<&str>,
    secure: bool,
) -> Result<RawWebSocket, WsError> {
    let port = if secure { 443 } else { 80 };
    ws_connect_once_port(dial_addr, port, domain, path, timeout, sni, secure).await
}

pub async fn ws_connect_once_port(
    dial_addr: &str,
    port: u16,
    domain: &str,
    path: &str,
    timeout: Duration,
    sni: Option<&str>,
    secure: bool,
) -> Result<RawWebSocket, WsError> {
    if dial_addr.is_empty() {
        return Err(WsError::Other("empty dial address".to_string()));
    }
    let path = if path.is_empty() { WS_PATH } else { path };
    let target_addr = format_host_port(dial_addr, port);

    let raw_conn = match tokio::time::timeout(timeout, TcpStream::connect(&target_addr)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(WsError::Io(e)),
        Err(_) => return Err(WsError::Timeout),
    };
    set_sock_opts(&raw_conn);

    if !secure {
        let (read_half, write_half) = tokio::io::split(raw_conn);
        let mut ws = handshake_plain(read_half, write_half, domain, path, timeout).await?;
        ws.remote_addr = dial_addr.to_string();
        ws.remote_port = port;
        return Ok(ws);
    }

    let sni_name = sni.unwrap_or(domain);
    let use_fronting = sni.is_some();
    let cfg = if use_fronting {
        TLS_CONFIG_FRONTING.clone()
    } else {
        TLS_CONFIG.clone()
    };
    let connector = TlsConnector::from(cfg);
    let sni_server = server_name(sni_name);

    let handshake_timeout = ws_handshake_timeout(timeout);
    let dpi_stream = DpiBypassStream::new(raw_conn);
    let tls_conn =
        match tokio::time::timeout(handshake_timeout, connector.connect(sni_server, dpi_stream)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                if e.kind() != std::io::ErrorKind::ConnectionReset {
                    ldebug!(" ws tls fail {} via {}:{}: {}", domain, dial_addr, port, e);
                }
                return Err(WsError::Io(e));
            }
            Err(_) => {
                ldebug!(" ws tls fail {} via {}:{}: timeout", domain, dial_addr, port);
                return Err(WsError::Timeout);
            }
        };

    let (read_half, write_half) = tokio::io::split(tls_conn);
    let mut ws = handshake_tls(read_half, write_half, domain, path, timeout).await?;
    ws.remote_addr = dial_addr.to_string();
    ws.remote_port = port;
    Ok(ws)
}

// wsConnect: пытается ip, при необходимости резолвит DoH
pub async fn ws_connect(
    ip: &str,
    domain: &str,
    path: &str,
    timeout: f64,
) -> Result<RawWebSocket, WsError> {
    ws_connect_full_opts(ip, domain, path, timeout, None, None).await
}

pub async fn ws_connect_full_opts(
    ip: &str,
    domain: &str,
    path: &str,
    timeout: f64,
    sni: Option<&str>,
    secure_opt: Option<bool>,
) -> Result<RawWebSocket, WsError> {
    let path = if path.is_empty() { WS_PATH } else { path };
    let attempt_timeout = ws_connect_timeout(timeout);
    let secure = secure_opt.unwrap_or(!DISABLE_SECURE.load(Ordering::Relaxed));

    let primary_addr = if ip.trim().is_empty() {
        domain.to_string()
    } else {
        ip.trim().to_string()
    };

    match ws_connect_once_full(&primary_addr, domain, path, attempt_timeout, sni, secure).await {
        Ok(ws) => return Ok(ws),
        Err(e) => {
            if primary_addr == domain && primary_addr.parse::<IpAddr>().is_err() {
                if let Some(resolved) = crate::cfproxy::resolve_doh(domain).await {
                    if !resolved.is_empty() && resolved != primary_addr {
                        return ws_connect_once_full(
                            &resolved,
                            domain,
                            path,
                            attempt_timeout,
                            sni,
                            secure,
                        )
                        .await;
                    }
                }
            }
            Err(e)
        }
    }
}

// connectOneWS: перебор доменов (pool refill path, fronting-first как в оригинале)
pub async fn connect_one_ws(ip: &str, domains: &[String]) -> Option<RawWebSocket> {
    connect_one_ws_full(ip, domains, WS_PATH, WS_POOL_CONNECT_TIMEOUT).await
}

pub async fn connect_one_ws_full(
    ip: &str,
    domains: &[String],
    path: &str,
    timeout: f64,
) -> Option<RawWebSocket> {
    // Поведение pool._connect_one: сначала fronting (sni=sprinthost.ru),
    // затем обычное соединение; Timeout/Reset → fronting fallback.
    let fronting_first = crate::proxy::ws_pool_fronting_first();
    for d in domains {
        if fronting_first {
            if let Some(ws) = connect_fronted(ip, d, path).await {
                return Some(ws);
            }
        }
        match ws_connect_full_opts(ip, d, path, timeout, None, Some(true)).await {
            Ok(ws) => {
                crate::proxy::set_ws_pool_fronting_first(false);
                return Some(ws);
            }
            Err(WsError::Timeout) | Err(WsError::Io(_)) => {
                if fronting_first {
                    // как в оригинале: при fronting-first таймаут → сразу fronted
                    return connect_fronted(ip, d, path).await;
                }
                if let Some(ws) = connect_fronted(ip, d, path).await {
                    return Some(ws);
                }
            }
            Err(WsError::Handshake(h)) => {
                if h.is_redirect() {
                    continue;
                }
                return None;
            }
            Err(_) => return None,
        }
    }
    None
}

static ACTIVE_FRONTING_IDX: AtomicUsize = AtomicUsize::new(0);

pub async fn connect_fronted(ip: &str, domain: &str, path: &str) -> Option<RawWebSocket> {
    let path = if path.is_empty() { WS_PATH } else { path };
    let total_snis = FRONTING_SNIS.len();
    if total_snis == 0 {
        return None;
    }
    let start_idx = ACTIVE_FRONTING_IDX.load(Ordering::Relaxed) % total_snis;

    // Пробуем активный SNI и при неудаче перебираем остальные из пула FRONTING_SNIS
    for offset in 0..total_snis.min(3) {
        let idx = (start_idx + offset) % total_snis;
        let sni_candidate = FRONTING_SNIS[idx];
        match ws_connect_full_opts(
            ip,
            domain,
            path,
            WS_POOL_FRONTING_TIMEOUT,
            Some(sni_candidate),
            Some(true),
        )
        .await
        {
            Ok(ws) => {
                ACTIVE_FRONTING_IDX.store(idx, Ordering::Relaxed);
                STATS.connections_fronting.fetch_add(1, Ordering::Relaxed);
                crate::proxy::set_ws_pool_fronting_first(true);
                return Some(ws);
            }
            Err(WsError::Handshake(h)) if h.is_redirect() => {
                // 302 редирект означает, что TLS прошёл, но сам DC не принимает WS для этого запроса
                ACTIVE_FRONTING_IDX.store(idx, Ordering::Relaxed);
                return None;
            }
            Err(_) => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_host_port_ipv4_and_ipv6() {
        assert_eq!(format_host_port("104.16.132.229", 443), "104.16.132.229:443");
        assert_eq!(format_host_port("kws2.web.telegram.org", 443), "kws2.web.telegram.org:443");
        assert_eq!(
            format_host_port("2606:4700::6810:84e5", 443),
            "[2606:4700::6810:84e5]:443"
        );
        assert_eq!(
            format_host_port("[2606:4700::6810:84e5]", 8443),
            "[2606:4700::6810:84e5]:8443"
        );
    }

    #[test]
    fn test_split_tls_client_hello_chunks() {
        // Собираем синтетический TLS ClientHello рекорд с SNI "kws2.web.telegram.org"
        let mut payload = vec![0x01u8]; // Handshake type ClientHello
        payload.extend_from_slice(&[0x00, 0x00, 0x40]); // length placeholder
        payload.extend_from_slice(&[0x03, 0x03]); // TLS 1.2
        payload.extend_from_slice(&[0xAA; 32]); // 32 bytes Random
        payload.push(0x00); // session id len 0
        payload.extend_from_slice(b"xxxxkws2.web.telegram.orgyyyy");

        let rec_len = payload.len();
        let mut record = vec![
            0x16,
            0x03,
            0x01,
            ((rec_len >> 8) & 0xFF) as u8,
            (rec_len & 0xFF) as u8,
        ];
        record.extend_from_slice(&payload);

        let chunks = split_tls_client_hello_chunks(&record).expect("must split ClientHello");
        assert_eq!(chunks.len(), 3);

        let mut reassembled = Vec::new();
        for c in &chunks {
            assert_eq!(c[0], 0x16);
            assert_eq!(c[1], 0x03);
            assert_eq!(c[2], 0x01);
            let l = ((c[3] as usize) << 8) | (c[4] as usize);
            assert_eq!(c.len(), 5 + l);
            reassembled.extend_from_slice(&c[5..]);
        }
        // Полезная нагрузка после объединения всех 3 TLS-рекордов должна совпадать 1-в-1
        assert_eq!(reassembled, payload);
        // Ни один из фрагментов не должен содержать полный SNI целиком
        for c in &chunks {
            let s = String::from_utf8_lossy(&c[5..]);
            assert!(!s.contains("kws2.web.telegram.org"));
        }
    }
}
