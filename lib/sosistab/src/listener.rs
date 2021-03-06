use crate::*;
use crate::{
    chan::recv_many,
    session::{Session, SessionConfig},
};
use bytes::Bytes;
use indexmap::IndexMap;
use msg::HandshakeFrame::*;
use parking_lot::RwLock;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use smol::channel::{Receiver, Sender};
use smol::net::AsyncToSocketAddrs;
use std::collections::HashMap;
use std::sync::Arc;
use std::{net::SocketAddr, time::Instant};

pub struct Listener {
    accepted: Receiver<Session>,
    local_addr: SocketAddr,
    _task: smol::Task<Option<()>>,
}

impl Listener {
    /// Accepts a session. This function must be repeatedly called for the entire Listener to make any progress.
    #[tracing::instrument(skip(self))]
    pub async fn accept_session(&self) -> Option<Session> {
        self.accepted.recv().await.ok()
    }
    /// Creates a new listener given the parameters.
    pub async fn listen(
        addr: impl AsyncToSocketAddrs,
        long_sk: x25519_dalek::StaticSecret,
    ) -> Self {
        // let addr = async_net::resolve(addr).await;
        let socket = runtime::new_udp_socket_bind(addr).await.unwrap();
        let local_addr = socket.get_ref().local_addr().unwrap();
        let cookie = crypt::Cookie::new((&long_sk).into());
        let (send, recv) = smol::channel::unbounded();
        let task = runtime::spawn(
            ListenerActor {
                socket: Arc::new(socket),
                cookie,
                long_sk,
            }
            .run(send),
        );
        Listener {
            accepted: recv,
            local_addr,
            _task: task,
        }
    }

    /// Gets the local address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

// recently seen tracker
struct RecentFilter {
    curr_bloom: bloomfilter::Bloom<[u8]>,
    last_bloom: bloomfilter::Bloom<[u8]>,
    curr_time: Instant,
}

impl RecentFilter {
    fn new() -> Self {
        RecentFilter {
            curr_bloom: bloomfilter::Bloom::new_for_fp_rate(100000, 0.01),
            last_bloom: bloomfilter::Bloom::new_for_fp_rate(100000, 0.01),
            curr_time: Instant::now(),
        }
    }

    fn check(&mut self, val: &[u8]) -> bool {
        if Instant::now()
            .saturating_duration_since(self.curr_time)
            .as_secs()
            > 600
        {
            std::mem::swap(&mut self.curr_bloom, &mut self.last_bloom);
            self.curr_bloom.clear()
        }
        !(self.curr_bloom.check_and_set(val) || self.last_bloom.check(val))
    }
}

type ShardedAddrs = IndexMap<u8, SocketAddr>;

struct ListenerActor {
    socket: Arc<dyn Backhaul>,
    cookie: crypt::Cookie,
    long_sk: x25519_dalek::StaticSecret,
}
impl ListenerActor {
    #[allow(clippy::mutable_key_type)]
    #[tracing::instrument(skip(self))]
    async fn run(self, accepted: Sender<Session>) -> Option<()> {
        // replay filter for globally-encrypted stuff
        let mut curr_filter = RecentFilter::new();
        // session table
        let mut session_table = SessionTable::default();
        // channel for dropping sessions
        let (send_dead, recv_dead) = smol::channel::unbounded();

        let token_key = {
            let mut buf = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut buf);
            buf
        };

        let socket = self.socket;

        // two possible events
        enum Evt {
            NewRecv((Bytes, SocketAddr)),
            DeadSess(Bytes),
        }

        loop {
            smol::future::yield_now().await;
            let event = smol::future::race(
                async { Some(Evt::NewRecv(socket.recv_from().await.ok()?)) },
                async { Some(Evt::DeadSess(recv_dead.recv().await.ok()?)) },
            );
            match event.await? {
                Evt::DeadSess(resume_token) => {
                    tracing::trace!("removing existing session!");
                    session_table.delete(resume_token).await;
                }
                Evt::NewRecv((buffer, addr)) => {
                    // first we attempt to map this to an existing session
                    if let Some((sess, sess_crypt)) = session_table.lookup(addr) {
                        // try feeding it into the session
                        if let Some(dframe) = sess_crypt.pad_decrypt::<msg::DataFrame>(&buffer) {
                            drop(sess.send(dframe).await);
                            continue;
                        } else {
                            tracing::trace!("{} NOT associated with existing session", addr);
                        }
                    }
                    if !curr_filter.check(&buffer) {
                        tracing::warn!("discarding replay attempt with len {}", buffer.len());
                        continue;
                    }
                    // we know it's not part of an existing session then. we decrypt it under the current key
                    let s2c_key = self.cookie.generate_s2c().next().unwrap();
                    for possible_key in self.cookie.generate_c2s() {
                        let crypter = crypt::StdAEAD::new(&possible_key);
                        if let Some(handshake) = crypter.pad_decrypt::<msg::HandshakeFrame>(&buffer)
                        {
                            match handshake {
                                ClientHello {
                                    long_pk,
                                    eph_pk,
                                    version,
                                } => {
                                    if version != 1 {
                                        tracing::warn!(
                                            "got packet with incorrect version {}",
                                            version
                                        );
                                        break;
                                    }
                                    // generate session key
                                    let my_eph_sk =
                                        x25519_dalek::StaticSecret::new(&mut rand::rngs::OsRng {});
                                    let token = TokenInfo {
                                        sess_key: crypt::triple_ecdh(
                                            &self.long_sk,
                                            &my_eph_sk,
                                            &long_pk,
                                            &eph_pk,
                                        )
                                        .as_bytes()
                                        .to_vec()
                                        .into(),
                                        init_time_ms: std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis()
                                            as u64,
                                    }
                                    .encrypt(&token_key);
                                    let reply = msg::HandshakeFrame::ServerHello {
                                        long_pk: (&self.long_sk).into(),
                                        eph_pk: (&my_eph_sk).into(),
                                        resume_token: token,
                                    };
                                    let reply =
                                        crypt::StdAEAD::new(&s2c_key).pad_encrypt(&reply, 1000);
                                    socket.send_to(reply, addr).await.ok()?;
                                    tracing::trace!("replied to ClientHello from {}", addr);
                                }
                                ClientResume {
                                    resume_token,
                                    shard_id,
                                } => {
                                    tracing::trace!("Got ClientResume-{} from {}!", shard_id, addr);
                                    // first check whether we know about the resume token
                                    if !session_table
                                        .rebind(addr, shard_id, resume_token.clone())
                                        .await
                                    {
                                        tracing::trace!("ClientResume from {} is new!", addr);
                                        let tokinfo = TokenInfo::decrypt(&token_key, &resume_token);
                                        if let Some(tokinfo) = tokinfo {
                                            let up_key = blake3::keyed_hash(
                                                crypt::UP_KEY,
                                                &tokinfo.sess_key,
                                            );
                                            let dn_key = blake3::keyed_hash(
                                                crypt::DN_KEY,
                                                &tokinfo.sess_key,
                                            );
                                            let up_aead = crypt::StdAEAD::new(up_key.as_bytes());
                                            let dn_aead = crypt::StdAEAD::new(dn_key.as_bytes());
                                            let socket = socket.clone();
                                            let (session_input, session_input_recv) =
                                                smol::channel::bounded(100);
                                            // create session
                                            let (session_output_send, session_output_recv) =
                                                smol::channel::bounded::<msg::DataFrame>(1000);
                                            let mut locked_addrs = IndexMap::new();
                                            locked_addrs.insert(shard_id, addr);
                                            // send for poll
                                            let locked_addrs = Arc::new(RwLock::new(locked_addrs));
                                            let output_poller = {
                                                let locked_addrs = locked_addrs.clone();
                                                runtime::spawn(async move {
                                                    let mut ctr = 0u8;
                                                    loop {
                                                        match recv_many(&session_output_recv).await
                                                        {
                                                            Ok(dff) => {
                                                                let remote_addr = loop {
                                                                    let addrs = locked_addrs.read();
                                                                    assert!(!addrs.is_empty());
                                                                    ctr = ctr.wrapping_add(1);
                                                                    if let Some((_, remote_addr)) =
                                                                        addrs.get_index(
                                                                            (ctr % (addrs.len()
                                                                                as u8))
                                                                                as usize,
                                                                        )
                                                                    {
                                                                        break *remote_addr;
                                                                    }
                                                                };
                                                                let encrypted: Vec<_> = dff
                                                                    .into_iter()
                                                                    .map(|df| {
                                                                        (
                                                                            dn_aead.pad_encrypt(
                                                                                &df, 1000,
                                                                            ),
                                                                            remote_addr,
                                                                        )
                                                                    })
                                                                    .collect();
                                                                drop(
                                                                    socket
                                                                        .send_to_many(&encrypted)
                                                                        .await,
                                                                );
                                                            }
                                                            Err(_) => {
                                                                smol::future::pending::<()>().await
                                                            }
                                                        }
                                                    }
                                                })
                                            };
                                            let mut session = Session::new(SessionConfig {
                                                target_loss: 0.05,
                                                send_frame: session_output_send,
                                                recv_frame: session_input_recv,
                                                recv_timeout: Duration::from_secs(3600),
                                            });
                                            let send_dead_clo = send_dead.clone();
                                            let resume_token_clo = resume_token.clone();
                                            session.on_drop(move || {
                                                drop(output_poller);
                                                drop(send_dead_clo.try_send(resume_token_clo))
                                            });
                                            // spawn a task that writes to the socket.
                                            session_table.new_sess(
                                                resume_token.clone(),
                                                session_input,
                                                up_aead,
                                                locked_addrs,
                                            );
                                            session_table
                                                .rebind(addr, shard_id, resume_token)
                                                .await;
                                            drop(accepted.send(session).await);
                                        } else {
                                            tracing::warn!(
                                                "ClientResume from {} can't be decrypted",
                                                addr
                                            );
                                        }
                                    }
                                }
                                _ => continue,
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenInfo {
    sess_key: Bytes,
    init_time_ms: u64,
}

impl TokenInfo {
    #[tracing::instrument]
    fn decrypt(key: &[u8], encrypted: &[u8]) -> Option<Self> {
        // first we decrypt
        let crypter = crypt::StdAEAD::new(key);
        let plain = crypter.decrypt(encrypted)?;
        bincode::deserialize(&plain).ok()
    }

    #[tracing::instrument]
    fn encrypt(&self, key: &[u8]) -> Bytes {
        let crypter = crypt::StdAEAD::new(key);
        let mut rng = rand::thread_rng();
        crypter.encrypt(
            &bincode::serialize(self).expect("must serialize"),
            rng.gen(),
        )
    }
}

type SessEntry = (
    Sender<msg::DataFrame>,
    crypt::StdAEAD,
    Arc<RwLock<ShardedAddrs>>,
);

#[derive(Default)]
struct SessionTable {
    token_to_sess: HashMap<Bytes, SessEntry>,
    addr_to_token: HashMap<SocketAddr, Bytes>,
}

impl SessionTable {
    #[tracing::instrument(skip(self))]
    async fn rebind(&mut self, addr: SocketAddr, shard_id: u8, token: Bytes) -> bool {
        if let Some((_, _, addrs)) = self.token_to_sess.get(&token) {
            let old = addrs.write().insert(shard_id, addr);
            tracing::trace!("binding {}=>{}", shard_id, addr);
            if let Some(old) = old {
                self.addr_to_token.remove(&old);
            }
            self.addr_to_token.insert(addr, token);
            true
        } else {
            false
        }
    }

    #[tracing::instrument(skip(self))]
    async fn delete(&mut self, token: Bytes) {
        if let Some((_, _, lock_addrs)) = self.token_to_sess.remove(&token) {
            for (_, addr) in lock_addrs.read().iter() {
                self.addr_to_token.remove(addr);
            }
        }
    }

    #[tracing::instrument(skip(self))]
    fn lookup(&self, addr: SocketAddr) -> Option<(&Sender<msg::DataFrame>, &crypt::StdAEAD)> {
        let token = self.addr_to_token.get(&addr)?;
        let (s, a, _) = self.token_to_sess.get(token)?;
        Some((s, a))
    }

    #[tracing::instrument(skip(self))]
    fn new_sess(
        &mut self,
        token: Bytes,
        sender: Sender<msg::DataFrame>,
        aead: crypt::StdAEAD,
        locked_addrs: Arc<RwLock<ShardedAddrs>>,
    ) {
        self.token_to_sess
            .insert(token, (sender, aead, locked_addrs));
    }
}
