//! Core danmaku client implementation.
//!
//! The [`DanmakuRecorder`] manages WebSocket connections, heartbeats,
//! message processing, and XML output for recording live stream chat.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{debug, error, info, warn};

use crate::error::{DanmakuError, Result};
use crate::output::xml::{XmlWriter, XmlWriterConfig};
use crate::protocols::{
    ConnectionInfo, ConnectionTransport, HeartbeatData, Platform, PlatformContext,
    RegistrationData, create_platform,
};

/// Configuration for the danmaku recorder.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// The live stream URL.
    pub url: String,
    /// Output file path template.
    pub output_file: PathBuf,
    /// Platform-specific context.
    pub context: PlatformContext,
    /// Whether to save raw message data.
    pub save_raw: bool,
    /// Whether to save detailed info.
    pub save_detail: bool,
}

impl RecorderConfig {
    /// Create a new recorder config.
    pub fn new(url: impl Into<String>, output_file: impl AsRef<Path>) -> Self {
        Self {
            url: url.into(),
            output_file: output_file.as_ref().to_path_buf(),
            context: PlatformContext::new(),
            save_raw: false,
            save_detail: false,
        }
    }

    /// Set the platform context.
    pub fn with_context(mut self, context: PlatformContext) -> Self {
        self.context = context;
        self
    }

    /// Enable raw data saving.
    pub fn with_raw(mut self, save_raw: bool) -> Self {
        self.save_raw = save_raw;
        self
    }

    /// Enable detailed info saving.
    pub fn with_detail(mut self, save_detail: bool) -> Self {
        self.save_detail = save_detail;
        self
    }
}

/// Commands that can be sent to the recorder.
#[derive(Debug)]
enum RecorderCommand {
    /// Save current file and optionally rename.
    Rolling {
        new_file_name: Option<PathBuf>,
        done: oneshot::Sender<Result<bool>>,
    },
    /// Stop recording.
    Stop,
}

/// How the background recorder task ended.
///
/// `start()` returns as soon as the task is spawned, so every failure after that point is
/// invisible to the caller unless it is reported here. The variants carry a stable
/// classification only — raw error text stays in the unchanged log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderExit {
    /// Stopped on request or the stream ended, and the XML file was finalized.
    Completed,
    /// Recording ended early; no further danmaku is produced for this session.
    Failed(RecorderFailure),
    /// The task neither finalized nor returned an error, i.e. it panicked or was cancelled.
    Aborted,
}

/// Stable classification of a terminal recorder failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderFailure {
    /// The XML output could not be created, written or finalized.
    Output,
    /// A connection, HTTP or transport failure the loop could not recover from.
    Connection,
    /// A protocol payload could not be decoded.
    Protocol,
    /// Anything else, including internal channel failures.
    Internal,
}

impl RecorderFailure {
    fn of(error: &DanmakuError) -> Self {
        match error {
            DanmakuError::Xml(_) | DanmakuError::Io(_) => Self::Output,
            DanmakuError::WebSocket(_) | DanmakuError::Http(_) | DanmakuError::ConnectionClosed => {
                Self::Connection
            }
            DanmakuError::Decode(_) | DanmakuError::Json(_) | DanmakuError::Compression(_) => {
                Self::Protocol
            }
            DanmakuError::UnsupportedPlatform(_)
            | DanmakuError::ChannelSend
            | DanmakuError::Stopped => Self::Internal,
        }
    }

    /// Stable lowercase code for the host's diagnostic vocabulary.
    pub fn code(self) -> &'static str {
        match self {
            Self::Output => "danmaku_output_failed",
            Self::Connection => "danmaku_connection_failed",
            Self::Protocol => "danmaku_protocol_failed",
            Self::Internal => "danmaku_internal_failed",
        }
    }
}

/// Callback invoked exactly once when the background task ends.
pub type ExitObserver = Arc<dyn Fn(RecorderExit) + Send + Sync>;

/// Reports the task's end exactly once, including the panicked or cancelled case in which no
/// branch of the task body runs to completion. The observer is isolated from the recorder: a
/// panic inside it must not escape into the task or abort the process during unwinding.
struct ExitReport {
    observer: Option<ExitObserver>,
    exit: RecorderExit,
}

impl ExitReport {
    fn new(observer: Option<ExitObserver>) -> Self {
        Self {
            observer,
            exit: RecorderExit::Aborted,
        }
    }

    fn settle(&mut self, exit: RecorderExit) {
        self.exit = exit;
    }
}

impl Drop for ExitReport {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.take() {
            let exit = self.exit;
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(exit)));
        }
    }
}

/// Handle for controlling a running recorder.
#[derive(Clone)]
pub struct RecorderHandle {
    cmd_tx: mpsc::Sender<RecorderCommand>,
    stop_tx: watch::Sender<bool>,
}

impl RecorderHandle {
    /// Stop the recorder.
    pub async fn stop(&self) -> Result<()> {
        let _ = self.stop_tx.send(true);
        let _ = self.cmd_tx.send(RecorderCommand::Stop).await;
        Ok(())
    }

    /// Save current recording and optionally rename the file.
    pub async fn rolling(&self, new_file_name: Option<PathBuf>) -> Result<bool> {
        let (done, rx) = oneshot::channel();
        self.cmd_tx
            .send(RecorderCommand::Rolling {
                new_file_name,
                done,
            })
            .await
            .map_err(|_| DanmakuError::ChannelSend)?;
        rx.await.map_err(|_| DanmakuError::ChannelSend)?
    }
}

/// Danmaku recorder that manages the recording lifecycle.
pub struct DanmakuRecorder {
    config: RecorderConfig,
    platform: Arc<dyn Platform>,
    exit_observer: Option<ExitObserver>,
}

impl DanmakuRecorder {
    /// Create a new recorder for the given URL.
    pub fn new(config: RecorderConfig) -> Result<Self> {
        let platform = create_platform(&config.url)?;
        Ok(Self {
            config,
            platform: Arc::from(platform),
            exit_observer: None,
        })
    }

    /// Observe how the background task ends. Without an observer the recorder behaves exactly as
    /// before: the failure is only written to the existing log line.
    pub fn with_exit_observer(mut self, observer: ExitObserver) -> Self {
        self.exit_observer = Some(observer);
        self
    }

    /// Start recording in a background task.
    ///
    /// Returns a handle that can be used to control the recorder.
    pub fn start(self) -> RecorderHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (stop_tx, stop_rx) = watch::channel(false);

        let handle = RecorderHandle {
            cmd_tx,
            stop_tx: stop_tx.clone(),
        };

        let observer = self.exit_observer.clone();

        tokio::spawn(async move {
            let mut report = ExitReport::new(observer);
            match self.run(cmd_rx, stop_rx).await {
                Ok(()) | Err(DanmakuError::Stopped) => report.settle(RecorderExit::Completed),
                Err(e) => {
                    error!("Recorder error: {}", e);
                    report.settle(RecorderExit::Failed(RecorderFailure::of(&e)));
                }
            }
        });

        handle
    }

    /// Run the recorder loop.
    async fn run(
        self,
        mut cmd_rx: mpsc::Receiver<RecorderCommand>,
        mut stop_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        let platform_name = self.platform.name();
        info!(
            "Starting danmaku recording for {} - {}",
            platform_name, self.config.url
        );

        // Create XML writer
        let xml_config = XmlWriterConfig {
            save_raw: self.config.save_raw,
            save_detail: self.config.save_detail,
            save_interval: if self.config.save_raw { 300 } else { 10 },
        };

        let output_path = format_output_path(&self.config.output_file);
        let mut xml_writer = XmlWriter::new(&output_path, xml_config.clone())?;

        // Main loop with reconnection
        if is_polling_url(&self.config.url) {
            match self
                .poll_and_run(&mut cmd_rx, &mut stop_rx, &mut xml_writer, &xml_config)
                .await
            {
                Ok(()) | Err(DanmakuError::Stopped) => {}
                Err(e) => return Err(e),
            }
        } else {
            loop {
                if *stop_rx.borrow() {
                    break;
                }

                match self
                    .connect_and_run(&mut cmd_rx, &mut stop_rx, &mut xml_writer, &xml_config)
                    .await
                {
                    Ok(()) | Err(DanmakuError::Stopped) => break,
                    Err(e) => {
                        warn!(
                            "{}: Connection error: {}. Reconnecting in 30s...",
                            platform_name, e
                        );

                        let mut reconnect_sleep =
                            Box::pin(tokio::time::sleep(Duration::from_secs(30)));
                        loop {
                            tokio::select! {
                                _ = &mut reconnect_sleep => break,
                                _ = stop_rx.changed() => {
                                    if *stop_rx.borrow() {
                                        break;
                                    }
                                }
                                Some(command) = cmd_rx.recv() => {
                                    if handle_command(command, &self.config.output_file, &mut xml_writer, &xml_config)? {
                                        break;
                                    }
                                }
                            }

                            if *stop_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Finish XML file
        let has_messages = xml_writer.has_messages();
        let final_path = xml_writer.finish()?;
        if !has_messages {
            let _ = fs::remove_file(&final_path);
        }
        info!(
            "{}: Recording finished. Output: {:?}",
            platform_name, final_path
        );

        Ok(())
    }

    /// Connect to WebSocket and process messages.
    async fn poll_and_run(
        &self,
        cmd_rx: &mut mpsc::Receiver<RecorderCommand>,
        stop_rx: &mut watch::Receiver<bool>,
        xml_writer: &mut XmlWriter,
        xml_config: &XmlWriterConfig,
    ) -> Result<()> {
        let platform_name = self.platform.name();
        let mut context = self.config.context.clone();
        let conn_info = self
            .platform
            .get_connection_info(&self.config.url, &context)
            .await?;
        let continuation = conn_info
            .ws_url
            .strip_prefix("poll://youtube?continuation=")
            .ok_or_else(|| DanmakuError::Decode("Invalid polling connection info".to_string()))?;
        context
            .extra
            .insert("continuation".to_string(), continuation.to_string());

        let mut ticker = interval(self.platform.poll_interval());
        info!("{}: Started polling danmaku", platform_name);

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        return Err(DanmakuError::Stopped);
                    }
                }

                Some(command) = cmd_rx.recv() => {
                    if handle_command(command, &self.config.output_file, xml_writer, xml_config)? {
                        return Err(DanmakuError::Stopped);
                    }
                }

                _ = ticker.tick() => {
                    let events = self.platform.poll_messages(&self.config.url, &mut context).await?;
                    for event in events {
                        if let Err(e) = xml_writer.write_event(&event) {
                            warn!("Failed to write event: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// Connect to WebSocket and process messages.
    async fn connect_and_run(
        &self,
        cmd_rx: &mut mpsc::Receiver<RecorderCommand>,
        stop_rx: &mut watch::Receiver<bool>,
        xml_writer: &mut XmlWriter,
        xml_config: &XmlWriterConfig,
    ) -> Result<()> {
        let platform_name = self.platform.name();

        // Get connection info
        let conn_info = self
            .platform
            .get_connection_info(&self.config.url, &self.config.context)
            .await?;

        match conn_info.transport {
            ConnectionTransport::WebSocket => {
                self.run_websocket_connection(
                    conn_info,
                    cmd_rx,
                    stop_rx,
                    xml_writer,
                    xml_config,
                    platform_name,
                )
                .await
            }
            ConnectionTransport::Tcp => {
                self.run_tcp_connection(
                    conn_info,
                    cmd_rx,
                    stop_rx,
                    xml_writer,
                    xml_config,
                    platform_name,
                )
                .await
            }
        }
    }

    async fn run_websocket_connection(
        &self,
        conn_info: ConnectionInfo,
        cmd_rx: &mut mpsc::Receiver<RecorderCommand>,
        stop_rx: &mut watch::Receiver<bool>,
        xml_writer: &mut XmlWriter,
        xml_config: &XmlWriterConfig,
        platform_name: &str,
    ) -> Result<()> {
        debug!("{}: Connecting to {}", platform_name, conn_info.ws_url);

        // Connect
        let ws_stream = connect_websocket(&conn_info, platform_name).await?;
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        info!("{}: Connected to WebSocket", platform_name);

        // Send registration data
        for reg_data in &conn_info.registration_data {
            let msg = match reg_data {
                RegistrationData::Text(text) => Message::Text(text.clone().into()),
                RegistrationData::Binary(data) => Message::Binary(data.clone().into()),
            };
            ws_sink.send(msg).await?;
        }

        // Get heartbeat config
        let heartbeat_config = self.platform.heartbeat_config();

        // Create heartbeat receiver
        let mut heartbeat_rx = if let Some(ref hb_data) = heartbeat_config.data {
            let hb_data = hb_data.clone();
            let interval_duration = heartbeat_config.interval;
            let (hb_tx, hb_rx) = mpsc::channel::<Message>(1);

            tokio::spawn(async move {
                let mut ticker = interval(interval_duration);
                loop {
                    ticker.tick().await;
                    let msg = match &hb_data {
                        HeartbeatData::Text(text) => Message::Text(text.clone().into()),
                        HeartbeatData::Binary(data) => Message::Binary(data.clone().into()),
                    };
                    if hb_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            Some(hb_rx)
        } else {
            None
        };

        // Main message loop
        loop {
            tokio::select! {
                // Check stop signal
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        return Err(DanmakuError::Stopped);
                    }
                }

                // Handle heartbeat
                hb_msg = async {
                    match heartbeat_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => futures::future::pending().await,
                    }
                } => {
                    if let Some(msg) = hb_msg {
                        ws_sink.send(msg).await?;
                    }
                }

                // Handle recorder commands
                Some(command) = cmd_rx.recv() => {
                    if handle_command(command, &self.config.output_file, xml_writer, xml_config)? {
                        return Err(DanmakuError::Stopped);
                    }
                }

                // Handle WebSocket message
                ws_msg = ws_stream.next() => {
                    match ws_msg {
                        Some(Ok(msg)) => {
                            let data = match msg {
                                Message::Text(text) => text.as_str().as_bytes().to_vec(),
                                Message::Binary(data) => data.to_vec(),
                                Message::Ping(data) => {
                                    ws_sink.send(Message::Pong(data)).await?;
                                    continue;
                                }
                                Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    return Err(DanmakuError::ConnectionClosed);
                                }
                                _ => continue,
                            };

                            // Decode message
                            match self.platform.decode_message(&data) {
                                Ok(result) => {
                                    // Write decoded events
                                    for event in result.events {
                                        if let Err(e) = xml_writer.write_event(&event) {
                                            warn!("Failed to write event: {}", e);
                                        }
                                    }

                                    // Send ack if needed
                                    if let Some(ack) = result.ack {
                                        if result.ack_is_text {
                                            let text = String::from_utf8(ack)
                                                .map_err(|e| DanmakuError::Decode(e.to_string()))?;
                                            ws_sink.send(Message::Text(text.into())).await?;
                                        } else {
                                            ws_sink.send(Message::Binary(ack.into())).await?;
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!("{}: Decode error: {}", platform_name, e);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            return Err(DanmakuError::WebSocket(e));
                        }
                        None => {
                            return Err(DanmakuError::ConnectionClosed);
                        }
                    }
                }
            }
        }
    }

    async fn run_tcp_connection(
        &self,
        conn_info: ConnectionInfo,
        cmd_rx: &mut mpsc::Receiver<RecorderCommand>,
        stop_rx: &mut watch::Receiver<bool>,
        xml_writer: &mut XmlWriter,
        xml_config: &XmlWriterConfig,
        platform_name: &str,
    ) -> Result<()> {
        debug!("{}: Connecting to {}", platform_name, conn_info.ws_url);

        let mut tcp_stream = connect_tcp(&conn_info, platform_name).await?;
        info!("{}: Connected to TCP danmaku endpoint", platform_name);

        for reg_data in &conn_info.registration_data {
            match reg_data {
                RegistrationData::Text(text) => tcp_stream.write_all(text.as_bytes()).await?,
                RegistrationData::Binary(data) => tcp_stream.write_all(data).await?,
            }
        }

        let heartbeat_config = self.platform.heartbeat_config();
        let mut heartbeat_rx = if let Some(ref hb_data) = heartbeat_config.data {
            let hb_data = hb_data.clone();
            let interval_duration = heartbeat_config.interval;
            let (hb_tx, hb_rx) = mpsc::channel::<Vec<u8>>(1);

            tokio::spawn(async move {
                let mut ticker = interval(interval_duration);
                loop {
                    ticker.tick().await;
                    let data = match &hb_data {
                        HeartbeatData::Text(text) => text.as_bytes().to_vec(),
                        HeartbeatData::Binary(data) => data.clone(),
                    };
                    if hb_tx.send(data).await.is_err() {
                        break;
                    }
                }
            });

            Some(hb_rx)
        } else {
            None
        };

        let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        return Err(DanmakuError::Stopped);
                    }
                }

                hb_data = async {
                    match heartbeat_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => futures::future::pending().await,
                    }
                } => {
                    if let Some(data) = hb_data {
                        tcp_writer.write_all(&data).await?;
                    }
                }

                Some(command) = cmd_rx.recv() => {
                    if handle_command(command, &self.config.output_file, xml_writer, xml_config)? {
                        return Err(DanmakuError::Stopped);
                    }
                }

                frame = read_tcp_frame(&mut tcp_reader) => {
                    let frame = frame?;
                    match self.platform.decode_message(&frame) {
                        Ok(result) => {
                            for event in result.events {
                                if let Err(e) = xml_writer.write_event(&event) {
                                    warn!("Failed to write event: {}", e);
                                }
                            }

                            if let Some(ack) = result.ack {
                                tcp_writer.write_all(&ack).await?;
                            }
                        }
                        Err(e) => {
                            debug!("{}: Decode error: {}", platform_name, e);
                        }
                    }
                }
            }
        }
    }
}

async fn connect_tcp(conn_info: &ConnectionInfo, platform_name: &str) -> Result<TcpStream> {
    let urls = std::iter::once(&conn_info.ws_url).chain(conn_info.fallback_ws_urls.iter());
    let mut last_error = None;

    for (index, tcp_url) in urls.enumerate() {
        let addr = parse_tcp_addr(tcp_url)?;
        debug!("{}: Connecting to {}", platform_name, tcp_url);

        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                if index > 0 {
                    warn!(
                        "{}: Primary TCP endpoint failed, fell back to {}",
                        platform_name, tcp_url
                    );
                }
                return Ok(stream);
            }
            Err(err) => {
                warn!(
                    "{}: TCP connect to {} failed: {}",
                    platform_name, tcp_url, err
                );
                last_error = Some(err);
            }
        }
    }

    Err(DanmakuError::Io(last_error.unwrap_or_else(|| {
        std::io::Error::other("no TCP endpoints configured")
    })))
}

fn parse_tcp_addr(url: &str) -> Result<String> {
    url.strip_prefix("tcp://")
        .filter(|addr| !addr.is_empty())
        .map(str::to_string)
        .ok_or_else(|| DanmakuError::Decode(format!("Invalid TCP endpoint: {url}")))
}

async fn read_tcp_frame(reader: &mut tokio::net::tcp::OwnedReadHalf) -> Result<Vec<u8>> {
    let mut header = [0u8; 12];
    reader.read_exact(&mut header).await?;

    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if length < 8 {
        return Err(DanmakuError::Decode(format!(
            "Invalid TCP frame length: {length}"
        )));
    }

    let mut frame = Vec::with_capacity(4 + length);
    frame.extend_from_slice(&header);
    frame.resize(4 + length, 0);
    reader.read_exact(&mut frame[12..]).await?;
    Ok(frame)
}

fn platform_tls_connector() -> Result<Connector> {
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .map_err(|e| DanmakuError::Decode(e.to_string()))?
        .with_platform_verifier()
        .map_err(|e| DanmakuError::Decode(e.to_string()))?
        .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}

async fn connect_websocket(
    conn_info: &ConnectionInfo,
    platform_name: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let urls = std::iter::once(&conn_info.ws_url).chain(conn_info.fallback_ws_urls.iter());
    let mut last_error = None;

    for (index, ws_url) in urls.enumerate() {
        debug!("{}: Connecting to {}", platform_name, ws_url);

        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| DanmakuError::Decode(e.to_string()))?;

        for (key, value) in conn_info.headers.iter() {
            request.headers_mut().insert(key.clone(), value.clone());
        }

        let connector = if ws_url.starts_with("wss://") {
            Some(platform_tls_connector()?)
        } else {
            None
        };

        match connect_async_tls_with_config(request, None, false, connector).await {
            Ok((ws_stream, _)) => {
                if index > 0 {
                    warn!(
                        "{}: Primary WebSocket failed, fell back to {}",
                        platform_name, ws_url
                    );
                }
                return Ok(ws_stream);
            }
            Err(err) => {
                warn!(
                    "{}: WebSocket connect to {} failed: {}",
                    platform_name, ws_url, err
                );
                last_error = Some(err);
            }
        }
    }

    Err(DanmakuError::WebSocket(last_error.unwrap_or_else(|| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::other(
            "no WebSocket endpoints configured",
        ))
    })))
}

fn is_polling_url(url: &str) -> bool {
    url.contains("youtube.com") || url.contains("youtu.be")
}

fn handle_command(
    command: RecorderCommand,
    template: &Path,
    xml_writer: &mut XmlWriter,
    xml_config: &XmlWriterConfig,
) -> Result<bool> {
    match command {
        RecorderCommand::Rolling {
            new_file_name,
            done,
        } => {
            let result = roll_writer(xml_writer, template, xml_config, new_file_name);
            let _ = done.send(result);
            Ok(false)
        }
        RecorderCommand::Stop => Ok(true),
    }
}

fn roll_writer(
    xml_writer: &mut XmlWriter,
    template: &Path,
    xml_config: &XmlWriterConfig,
    new_file_name: Option<PathBuf>,
) -> Result<bool> {
    let current_path = xml_writer.file_path().to_path_buf();
    xml_writer.finalize()?;
    let current_exists = current_path.exists();
    *xml_writer = XmlWriter::new(next_output_path(template), xml_config.clone())?;

    if !current_exists {
        return Ok(false);
    }

    if let Some(new_path) = new_file_name {
        if current_path != new_path {
            if let Some(parent) = new_path.parent() {
                fs::create_dir_all(parent)?;
            }
            if new_path.exists() {
                fs::remove_file(&new_path)?;
            }
            fs::rename(current_path, new_path)?;
        }
    }

    Ok(true)
}

fn next_output_path(template: &Path) -> PathBuf {
    let output_path = format_output_path(template);
    if !output_path.exists() {
        return output_path;
    }

    let parent = output_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let stem = output_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "danmaku".to_string());

    for index in 1.. {
        let candidate = parent.join(format!("{stem}_{index}.xml"));
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!()
}

/// Format output path with timestamp substitution.
fn format_output_path(template: &Path) -> PathBuf {
    let now = chrono::Local::now();
    let path_str = template.to_string_lossy();

    // Replace strftime-like patterns
    let formatted = now.format(&path_str).to_string();

    PathBuf::from(formatted).with_extension("xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_without_messages_renames_current_xml() {
        let dir = std::env::temp_dir().join(format!(
            "danmaku-roll-empty-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let template = dir.join("danmaku");
        let new_path = dir.join("segment.xml");
        let config = XmlWriterConfig::default();
        let mut writer = XmlWriter::new(format_output_path(&template), config.clone()).unwrap();
        let current_path = writer.file_path().to_path_buf();

        assert!(roll_writer(&mut writer, &template, &config, Some(new_path.clone())).unwrap());

        assert!(!current_path.exists());
        assert!(new_path.exists());
        let content = std::fs::read_to_string(&new_path).unwrap();
        assert!(content.contains("<i>"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rolling_missing_current_xml_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "danmaku-roll-missing-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let template = dir.join("danmaku");
        let new_path = dir.join("segment.xml");
        let config = XmlWriterConfig::default();
        let mut writer = XmlWriter::new(format_output_path(&template), config.clone()).unwrap();
        let current_path = writer.file_path().to_path_buf();
        std::fs::remove_file(&current_path).unwrap();

        assert!(roll_writer(&mut writer, &template, &config, Some(new_path.clone())).is_ok());

        assert!(!new_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_format_output_path() {
        let template = PathBuf::from("/tmp/test_%Y%m%d");
        let result = format_output_path(&template);
        assert!(result.to_string_lossy().contains("/tmp/test_"));
        assert!(result.extension().map(|e| e == "xml").unwrap_or(false));
    }

    fn recording_observer() -> (ExitObserver, Arc<std::sync::Mutex<Vec<RecorderExit>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let observer: ExitObserver = Arc::new(move |exit| sink.lock().unwrap().push(exit));
        (observer, seen)
    }

    #[test]
    fn failure_classification_follows_the_error_kind() {
        assert_eq!(
            RecorderFailure::of(&DanmakuError::Xml("write".into())),
            RecorderFailure::Output
        );
        assert_eq!(
            RecorderFailure::of(&DanmakuError::ConnectionClosed),
            RecorderFailure::Connection
        );
        assert_eq!(
            RecorderFailure::of(&DanmakuError::Decode("frame".into())),
            RecorderFailure::Protocol
        );
        assert_eq!(
            RecorderFailure::of(&DanmakuError::ChannelSend),
            RecorderFailure::Internal
        );
        assert_eq!(RecorderFailure::Output.code(), "danmaku_output_failed");
    }

    #[test]
    fn an_unsettled_report_is_an_abort_and_fires_once() {
        let (observer, seen) = recording_observer();
        drop(ExitReport::new(Some(observer.clone())));
        assert_eq!(*seen.lock().unwrap(), vec![RecorderExit::Aborted]);

        let mut report = ExitReport::new(Some(observer));
        report.settle(RecorderExit::Failed(RecorderFailure::Output));
        report.settle(RecorderExit::Completed);
        drop(report);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![RecorderExit::Aborted, RecorderExit::Completed]
        );
    }

    #[test]
    fn a_panicking_observer_does_not_escape_the_recorder() {
        let observer: ExitObserver = Arc::new(|_| panic!("observer is broken"));
        let mut report = ExitReport::new(Some(observer));
        report.settle(RecorderExit::Completed);
        drop(report);
    }

    /// The failure this closes: `start()` has already handed back a handle, so a recorder that
    /// dies while opening its output leaves the caller believing danmaku is being recorded.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_recorder_that_dies_after_start_reports_its_exit() {
        let (observer, seen) = recording_observer();
        let config = RecorderConfig::new(
            "https://www.twitch.tv/shroud",
            "/dev/null/unwritable/danmaku",
        );
        let handle = DanmakuRecorder::new(config)
            .expect("platform is supported")
            .with_exit_observer(observer)
            .start();

        for _ in 0..100 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            *seen.lock().unwrap(),
            vec![RecorderExit::Failed(RecorderFailure::Output)],
            "an unwritable output must be reported as a terminal output failure"
        );
        // The handle outlives the dead task; stopping it must stay harmless.
        handle.stop().await.expect("stop is infallible");
    }
}
