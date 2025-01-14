use bitbuffer::{BitError, BitRead, BitReadBuffer, BitReadStream, LittleEndian};
use notify::event::ModifyKind;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::{metadata, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::gameevent::GameEventMessage;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::message::MessagePacket;
use tf_demo_parser::demo::packet::Packet;
use tf_demo_parser::demo::parser::gamestateanalyser::GameStateAnalyser;
use tf_demo_parser::demo::parser::{DemoHandler, RawPacketStream};

pub struct DemoManager {
    previous_demos: Vec<OpenDemo>,
    current_demo: Option<OpenDemo>,
}

pub struct OpenDemo {
    pub file_path: PathBuf,
    pub header: Option<Header>,
    pub handler: DemoHandler<GameStateAnalyser>,
    pub bytes: Vec<u8>,
    pub offset: usize,
}

impl DemoManager {
    /// Create a new DemoManager
    pub fn new() -> DemoManager {
        DemoManager {
            previous_demos: Vec::new(),
            current_demo: None,
        }
    }

    /// Start tracking a new demo file. A demo must be being tracked before bytes can be appended.
    pub fn new_demo(&mut self, path: PathBuf) {
        if let Some(old) = self.current_demo.take() {
            self.previous_demos.push(old);
        }

        // TODO - Change to debug when demo monitoring defaults to on
        tracing::info!("Watching new demo: {:?}", path);

        self.current_demo = Some(OpenDemo {
            file_path: path,
            header: None,
            handler: DemoHandler::with_analyser(GameStateAnalyser::new()),
            bytes: Vec::new(),
            offset: 0,
        });
    }

    pub fn current_demo_path(&self) -> Option<&Path> {
        self.current_demo.as_ref().map(|d| d.file_path.as_path())
    }

    pub fn read_next_bytes(&mut self) {
        if let Some(demo) = self.current_demo.as_mut() {
            if let Err(e) = demo.read_next_bytes() {
                tracing::error!("Error when reading demo {:?}: {:?}", demo.file_path, e);
                tracing::error!("Demo is being abandoned");
                self.current_demo = None;
            }
        }
    }
}

impl OpenDemo {
    /// Append the provided bytes to the current demo being watched, and handle any packets
    pub fn read_next_bytes(&mut self) -> std::io::Result<()> {
        let current_metadata = metadata(&self.file_path)?;

        // Check there's actually data to read
        if current_metadata.len() < self.bytes.len() as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Demo has shortened. Something has gone wrong.",
            ));
        } else if current_metadata.len() == self.bytes.len() as u64 {
            return Ok(());
        }

        let mut file = File::open(&self.file_path)?;
        let last_size = self.bytes.len();

        file.seek(std::io::SeekFrom::Start(last_size as u64))?;
        let read_bytes = file.read_to_end(&mut self.bytes)?;

        if read_bytes > 0 {
            tracing::debug!("Got {} demo bytes", read_bytes);
            self.process_next_chunk()
        }

        Ok(())
    }

    fn process_next_chunk(&mut self) {
        // TODO - Change to debug when demo monitoring defaults to on
        tracing::info!("New demo length: {}", self.bytes.len());

        let buffer = BitReadBuffer::new(&self.bytes, LittleEndian);
        let mut stream = BitReadStream::new(buffer);
        stream.set_pos(self.offset).unwrap();

        // Parse header if there isn't one already
        if self.header.is_none() {
            match Header::read(&mut stream) {
                Ok(header) => {
                    self.handler.handle_header(&header);
                    self.header = Some(header);
                    self.offset = stream.pos();
                }
                Err(bitbuffer::BitError::NotEnoughData {
                    requested,
                    bits_left,
                }) => {
                    tracing::warn!("Tried to read header but there were not enough bits. Requested: {}, Remaining: {}", requested, bits_left);
                    return;
                }
                Err(e) => {
                    tracing::error!("Error reading demo header: {}", e);
                    return;
                }
            }
        }

        // Parse packets
        let mut packets: RawPacketStream = RawPacketStream::new(stream);
        loop {
            match packets.next(&self.handler.state_handler) {
                Ok(Some(packet)) => {
                    self.handle_packet(&packet);
                    self.handler.handle_packet(packet).unwrap();
                    self.offset = packets.pos();
                }
                Ok(None) => {
                    break;
                }
                Err(tf_demo_parser::ParseError::ReadError(BitError::NotEnoughData {
                    requested,
                    bits_left,
                })) => {
                    tracing::warn!("Tried to read packet but there were not enough bits. Requested: {}, Remaining: {}", requested, bits_left);
                    break;
                }
                Err(e) => {
                    tracing::error!("Error reading demo packet: {}", e);
                    return;
                }
            }
        }
    }

    fn handle_packet(&self, packet: &Packet) {
        if let Packet::Message(MessagePacket {
            tick: _,
            messages,
            meta: _,
        }) = packet
        {
            for m in messages {
                if let Message::GameEvent(GameEventMessage {
                    event_type_id: _,
                    event,
                }) = m
                {
                    match event {
                        GameEvent::VoteStarted(e) => {
                            tracing::info!("Vote started: {:?}", e);
                        }
                        GameEvent::VoteOptions(e) => {
                            tracing::info!("Vote options: {:?}", e);
                        }
                        GameEvent::VoteCast(e) => {
                            tracing::info!("Vote cast: {:?}", e);
                        }
                        GameEvent::VoteEnded(e) => {
                            tracing::info!("Vote ended: {:?}", e);
                        }
                        GameEvent::VotePassed(e) => {
                            tracing::info!("Vote passed: {:?}", e);
                        }
                        GameEvent::VoteFailed(e) => {
                            tracing::info!("Vote failed: {:?}", e);
                        }
                        GameEvent::VoteChanged(e) => {
                            tracing::info!("Vote changed: {:?}", e);
                        }
                        GameEvent::PlayerConnect(e) => {
                            tracing::info!("Player connect: {:?}", e);
                        }
                        GameEvent::PlayerConnectClient(e) => {
                            tracing::info!("Player connect client: {:?}", e);
                        }
                        GameEvent::PlayerInfo(e) => {
                            tracing::info!("Player info: {:?}", e);
                        }
                        GameEvent::Unknown(e) => {
                            tracing::info!("Unknown: {:?}", e);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

pub fn demo_loop(demo_path: PathBuf) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let config = Config::default().with_poll_interval(Duration::from_secs(2));

    let mut watcher: RecommendedWatcher = Watcher::new(
        Box::new(move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(err) => {
                tracing::error!("Error while watching for file changes: {}", err);
            }
        }),
        config,
    )?;

    watcher.watch(demo_path.as_path(), RecursiveMode::Recursive)?;

    // Create a tick interval to periodically check metadata
    let metadata_tick = Duration::from_secs(5);

    tracing::debug!("Demo loop started");

    let mut manager = DemoManager::new();
    loop {
        match rx.recv_timeout(metadata_tick) {
            Ok(event) => {
                let path = &event.paths[0];
                match event.kind {
                    notify::event::EventKind::Create(_) => {
                        if path.extension().map_or(false, |ext| ext == "dem") {
                            manager.new_demo(path.clone());
                        }
                    }
                    notify::event::EventKind::Modify(ModifyKind::Data(_)) => {
                        if manager
                            .current_demo_path()
                            .map(|p| p == path)
                            .unwrap_or(false)
                        {
                            manager.read_next_bytes();
                        } else if path.extension().map_or(false, |ext| ext == "dem") {
                            // A new demo can be started with the same name as a previous one, or the player can
                            // be already connected to a server and recording a demo when the application is run.
                            // This should catch those cases.
                            manager.new_demo(path.clone());
                            manager.read_next_bytes();
                        }
                    }
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                manager.read_next_bytes();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("Couldn't receive demo updates. Watcher died.");
            }
        }
    }
}
