use std::{
    net::SocketAddrV4,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Mutex as StdMutex,
    },
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use bytebuffer::{ByteBuffer, ByteReader};
use crypto_box::{
    aead::{Aead, AeadCore, OsRng},
    ChaChaBox,
};
use globed_shared::PROTOCOL_VERSION;
use log::{debug, warn};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};

use crate::{
    data::{
        packets::{client::*, match_packet, server::*, Packet, PacketWithId, PACKET_HEADER_LEN},
        types::{CryptoPublicKey, PlayerAccountData},
    },
    server::GameServer,
};

pub enum ServerThreadMessage {
    Packet(Vec<u8>),
    BroadcastVoice(VoiceBroadcastPacket),
}

pub struct GameServerThread {
    game_server: &'static GameServer,

    rx: Mutex<Receiver<ServerThreadMessage>>,
    tx: Sender<ServerThreadMessage>,
    awaiting_termination: AtomicBool,
    pub authenticated: AtomicBool,
    crypto_box: StdMutex<Option<ChaChaBox>>,

    peer: SocketAddrV4,
    pub account_id: AtomicI32,
    pub account_data: StdMutex<PlayerAccountData>,

    last_voice_packet: StdMutex<SystemTime>,
}

macro_rules! gs_require {
    ($cond:expr,$msg:literal) => {
        if !($cond) {
            return Err(anyhow!($msg));
        }
    };
}

macro_rules! gs_handler {
    ($self:ident,$name:ident,$pktty:ty,$pkt:ident,$code:expr) => {
        // Insanity if you ask me
        async fn $name(&$self, packet: &dyn Packet) -> anyhow::Result<Option<Box<dyn Packet>>> {
            let _tmp = packet.as_any().downcast_ref::<$pktty>();
            if _tmp.is_none() {
                return Err(anyhow!("failed to downcast packet"));
            }
            let $pkt = _tmp.unwrap();
            $code
        }
    };
}

macro_rules! gs_retpacket {
    ($code:expr) => {
        return Ok(Some(Box::new($code)))
    };
}

macro_rules! gs_disconnect {
    ($self:ident,$msg:expr) => {
        $self.terminate();
        gs_retpacket!(ServerDisconnectPacket { message: $msg })
    };
}

#[allow(unused_macros)]
macro_rules! gs_notice {
    ($msg:expr) => {
        gs_retpacket!(ServerNoticePacket { message: $msg })
    };
}

macro_rules! gs_needauth {
    ($self:ident) => {
        if !$self.authenticated.load(Ordering::Relaxed) {
            return Ok(Some(Box::new(ServerDisconnectPacket {
                message: "not logged in".to_string(),
            })));
        }
    };
}

impl GameServerThread {
    /* public api for the main server */

    pub fn new(peer: SocketAddrV4, game_server: &'static GameServer) -> Self {
        let (tx, rx) = mpsc::channel::<ServerThreadMessage>(8);
        Self {
            tx,
            rx: Mutex::new(rx),
            peer,
            crypto_box: StdMutex::new(None),
            account_id: AtomicI32::new(0),
            authenticated: AtomicBool::new(false),
            game_server,
            awaiting_termination: AtomicBool::new(false),
            account_data: StdMutex::new(PlayerAccountData::default()),
            last_voice_packet: StdMutex::new(SystemTime::now()),
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut rx = self.rx.lock().await;

        loop {
            if self.awaiting_termination.load(Ordering::Relaxed) {
                break;
            }

            match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
                Ok(Some(message)) => match self.handle_message(message).await {
                    Ok(_) => {}
                    Err(err) => warn!("{}", err.to_string()),
                },
                Ok(None) => break, // sender closed
                Err(_) => break,   // timeout
            };
        }

        Ok(())
    }

    pub async fn send_message(&self, data: ServerThreadMessage) -> anyhow::Result<()> {
        self.tx.send(data).await?;
        Ok(())
    }

    pub fn terminate(&self) {
        self.awaiting_termination.store(true, Ordering::Relaxed);
    }

    /* private utilities */

    async fn send_packet(&self, packet: &dyn Packet) -> anyhow::Result<()> {
        let serialized = self.serialize_packet(packet)?;
        self.game_server.socket.send_to(serialized.as_bytes(), self.peer).await?;

        Ok(())
    }

    fn parse_packet(&self, message: &[u8]) -> anyhow::Result<Box<dyn Packet>> {
        gs_require!(message.len() >= PACKET_HEADER_LEN, "packet is missing a header");

        let mut data = ByteReader::from_bytes(&message);

        let packet_id = data.read_u16()?;
        let encrypted = data.read_u8()? != 0u8;

        let packet = match_packet(packet_id);
        gs_require!(
            packet.is_some(),
            "packet was sent with an invalid id or the handler doesn't exist: {packet_id}"
        );

        let mut packet = packet.unwrap();
        if packet.get_encrypted() && !encrypted {
            gs_require!(false, "client sent a cleartext packet when expected an encrypted one");
        }

        if !encrypted {
            packet.decode_from_reader(&mut data)?;
            return Ok(packet);
        }

        let cbox = self.crypto_box.lock().unwrap();

        gs_require!(
            cbox.is_some(),
            "attempting to decode an encrypted packet when no cryptobox was initialized"
        );

        let encrypted_data = data.read_bytes(data.len() - data.get_rpos())?;
        let nonce = &encrypted_data[..24];
        let rest = &encrypted_data[24..];

        let cbox = cbox.as_ref().unwrap();
        let cleartext = cbox.decrypt(nonce.into(), rest)?;

        let mut packetbuf = ByteReader::from_bytes(&cleartext);
        packet.decode_from_reader(&mut packetbuf)?;

        Ok(packet)
    }

    fn serialize_packet(&self, packet: &dyn Packet) -> anyhow::Result<ByteBuffer> {
        let mut buf = ByteBuffer::new();
        buf.write_u16(packet.get_packet_id());
        buf.write_u8(if packet.get_encrypted() { 1u8 } else { 0u8 });

        if !packet.get_encrypted() {
            packet.encode(&mut buf);
            return Ok(buf);
        }

        let cbox = self.crypto_box.lock().unwrap();

        gs_require!(
            cbox.is_some(),
            "trying to send an encrypted packet when no cryptobox was initialized"
        );

        let mut cltxtbuf = ByteBuffer::new();
        packet.encode(&mut cltxtbuf);

        let cbox = cbox.as_ref().unwrap();
        let nonce = ChaChaBox::generate_nonce(&mut OsRng);

        let encrypted = cbox.encrypt(&nonce, cltxtbuf.as_bytes())?;

        buf.write_bytes(&nonce);
        buf.write_bytes(&encrypted);

        Ok(buf)
    }

    async fn handle_message(&self, message: ServerThreadMessage) -> anyhow::Result<()> {
        match message {
            ServerThreadMessage::Packet(message) => match self.parse_packet(&message) {
                Ok(packet) => match self.handle_packet(&*packet).await {
                    Ok(_) => {}
                    Err(err) => return Err(anyhow!("failed to handle packet: {}", err.to_string())),
                },
                Err(err) => return Err(anyhow!("failed to parse packet: {}", err.to_string())),
            },

            ServerThreadMessage::BroadcastVoice(voice_packet) => match self.send_packet(&voice_packet).await {
                Ok(_) => {}
                Err(err) => {
                    warn!("failed to broadcast voice packet: {}", err.to_string())
                }
            },
        }

        Ok(())
    }

    /* packet handlers */

    async fn handle_packet(&self, packet: &dyn Packet) -> anyhow::Result<()> {
        let response = match packet.get_packet_id() {
            /* connection related */
            PingPacket::PACKET_ID => self.handle_ping(packet).await,
            CryptoHandshakeStartPacket::PACKET_ID => self.handle_crypto_handshake(packet).await,
            KeepalivePacket::PACKET_ID => self.handle_keepalive(packet).await,
            LoginPacket::PACKET_ID => self.handle_login(packet).await,
            DisconnectPacket::PACKET_ID => self.handle_disconnect(packet).await,

            /* game related */
            SyncIconsPacket::PACKET_ID => self.handle_sync_icons(packet).await,
            RequestProfilesPacket::PACKET_ID => self.handle_request_profiles(packet).await,
            VoicePacket::PACKET_ID => self.handle_voice(packet).await,
            x => Err(anyhow!("No handler for packet id {x}")),
        }?;

        if let Some(response_packet) = response {
            self.send_packet(&*response_packet).await?;
        }

        Ok(())
    }

    gs_handler!(self, handle_ping, PingPacket, packet, {
        gs_retpacket!(PingResponsePacket {
            id: packet.id,
            player_count: self.game_server.state.player_count.load(Ordering::Relaxed),
        });
    });

    gs_handler!(self, handle_crypto_handshake, CryptoHandshakeStartPacket, packet, {
        match packet.protocol {
            p if p > PROTOCOL_VERSION => {
                gs_disconnect!(
                    self,
                    format!(
                        "Outdated server! You are running protocol v{p} while the server is still on v{PROTOCOL_VERSION}.",
                    )
                );
            }
            p if p < PROTOCOL_VERSION => {
                gs_disconnect!(
                    self,
                    format!(
                        "Outdated client! Please update the mod in order to connect to the server. Client protocol version: v{p}, server: v{PROTOCOL_VERSION}",
                    )
                );
            }
            _ => {}
        }

        let mut cbox = self.crypto_box.lock().unwrap();

        // as ServerThread is now tied to the SocketAddrV4 and not account id like in globed v0
        // erroring here is not a concern, even if the user's game crashes without a disconnect packet,
        // they would have a new randomized port when they restart and this would never fail.
        gs_require!(cbox.is_none(), "attempting to initialize a cryptobox twice");

        let sbox = ChaChaBox::new(&packet.key.pubkey, &self.game_server.secret_key);
        *cbox = Some(sbox);

        gs_retpacket!(CryptoHandshakeResponsePacket {
            key: CryptoPublicKey {
                pubkey: self.game_server.secret_key.public_key().clone()
            }
        });
    });

    gs_handler!(self, handle_keepalive, KeepalivePacket, _packet, {
        gs_needauth!(self);

        gs_retpacket!(KeepaliveResponsePacket {
            player_count: self.game_server.state.player_count.load(Ordering::Relaxed)
        })
    });

    gs_handler!(self, handle_login, LoginPacket, packet, {
        // lets verify the given token
        let state = self.game_server.state.read().await;
        let client = state.http_client.clone();
        let central_url = state.central_url.clone();
        let pw = state.central_pw.clone();
        drop(state);

        let url = format!("{central_url}gs/verify");

        let response = client
            .post(url)
            .query(&[
                ("account_id", packet.account_id.to_string()),
                ("token", packet.token.clone()),
                ("pw", pw),
            ])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        if !response.starts_with("status_ok:") {
            self.terminate();
            gs_retpacket!(LoginFailedPacket {
                message: format!("authentication failed: {}", response)
            });
        }

        let player_name = response.split_once(':').ok_or(anyhow!("central server is drunk"))?.1;

        self.authenticated.store(true, Ordering::Relaxed);
        self.account_id.store(packet.account_id, Ordering::Relaxed);
        self.game_server.state.player_count.fetch_add(1u32, Ordering::Relaxed); // increment player count

        let mut account_data = self.account_data.lock().unwrap();
        account_data.account_id = packet.account_id;
        account_data.name = player_name.to_string();

        debug!("Login successful from {player_name} ({})", packet.account_id);

        gs_retpacket!(LoggedInPacket {})
    });

    gs_handler!(self, handle_disconnect, DisconnectPacket, _packet, {
        self.terminate();
        return Ok(None);
    });

    /* game related */

    gs_handler!(self, handle_sync_icons, SyncIconsPacket, packet, {
        gs_needauth!(self);

        let mut account_data = self.account_data.lock().unwrap();
        account_data.icons.clone_from(&packet.icons);
        Ok(None)
    });

    gs_handler!(self, handle_request_profiles, RequestProfilesPacket, packet, {
        gs_needauth!(self);

        gs_retpacket!(PlayerProfilesPacket {
            profiles: self.game_server.gather_profiles(&packet.ids).await
        })
    });

    gs_handler!(self, handle_voice, VoicePacket, packet, {
        gs_needauth!(self);

        let accid = self.account_id.load(Ordering::Relaxed);
        if self.game_server.chat_blocked(accid) {
            debug!("blocking voice packet from {accid}");
            return Ok(None);
        }

        // check the throughput
        {
            let mut last_voice_packet = self.last_voice_packet.lock().unwrap();
            let now = SystemTime::now();
            let passed_time = now.duration_since(*last_voice_packet)?.as_millis();
            *last_voice_packet = now;

            let total_size = packet.data.opus_frames.iter().map(|frame| frame.len()).sum::<usize>();

            let throughput = (total_size as f32) / (passed_time as f32); // in kb/s

            debug!("voice packet throughput: {}kb/s", throughput);
            if throughput > 8f32 {
                warn!("rejecting a voice packet, throughput above the limit: {}kb/s", throughput);
                return Ok(None);
            }
        }

        let vpkt = VoiceBroadcastPacket {
            player_id: accid,
            data: packet.data.clone(),
        };

        self.game_server.broadcast_voice_packet(&vpkt).await?;

        Ok(None)
    });
}
