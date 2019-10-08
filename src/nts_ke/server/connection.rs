// This file is part of cfnts.
// Copyright (c) 2019, Cloudflare. All rights reserved.
// See LICENSE for licensing information.

//! NTS-KE server connection.

use byteorder::{BigEndian, WriteBytesExt};

use mio::tcp::{Shutdown, TcpStream};

use rustls::Session;

use slog::{debug, error, info};

use std::sync::{Arc, RwLock};
use std::io::{Read, Write};

use crate::cookie::{make_cookie, NTSKeys};
use crate::key_rotator::KeyRotator;
use crate::nts_ke::protocol::gen_key;
use crate::nts_ke::protocol::serialize_record;
use crate::nts_ke::protocol::{NtsKeRecord, NtsKeType};

use super::listener::KeServerListener;
use super::server::KeServerState;

// response uses the configuration and the keys and computes the response
// sent to the client.
fn response(keys: NTSKeys, rotator: &Arc<RwLock<KeyRotator>>, port: &u16) -> Vec<u8> {
    let mut response: Vec<u8> = Vec::new();
    let mut next_proto = NtsKeRecord {
        critical: true,
        record_type: NtsKeType::NextProtocolNegotiation,
        contents: vec![0, 0],
    };

    let mut aead_rec = NtsKeRecord {
        critical: false,
        record_type: NtsKeType::AEADAlgorithmNegotiation,
        contents: vec![0, 15],
    };

    let mut port_rec = NtsKeRecord {
        critical: false,
        record_type: NtsKeType::PortNegotiation,
        contents: vec![],
    };

    port_rec.contents.write_u16::<BigEndian>(*port).unwrap();

    let mut end_rec = NtsKeRecord {
        critical: true,
        record_type: NtsKeType::EndOfMessage,
        contents: vec![],
    };

    response.append(&mut serialize_record(&mut next_proto));
    response.append(&mut serialize_record(&mut aead_rec));
    let rotor = rotator.read().unwrap();
    let (key_id, actual_key) = rotor.latest_key_value();
    for _i in 1..8 {
        let cookie = make_cookie(keys, actual_key.as_ref(), key_id);
        let mut cookie_rec = NtsKeRecord {
            critical: false,
            record_type: NtsKeType::NewCookie,
            contents: cookie,
        };
        response.append(&mut serialize_record(&mut cookie_rec));
    }
    response.append(&mut serialize_record(&mut port_rec));
    response.append(&mut serialize_record(&mut end_rec));
    response
}

#[derive(Eq, PartialEq)]
enum KeServerConnState {
    /// The connection is just connected. The TLS handshake is not done yet.
    Connected,
    /// Doing the TLS handshake,
    TlsHandshaking,
    /// The TLS handshake is done. It's opened for requests now.
    Opened,
    /// The reponse is sent after getting a good request.
    ResponseSent,
    /// The connection is closed.
    Closed,
}

/// NTS-KE server TCP connection.
pub struct KeServerConn {
    /// Reference back to the corresponding `KeServer` state.
    server_state: Arc<KeServerState>,

    /// Kernel TCP stream.
    tcp_stream: TcpStream,

    /// The mio token for this connection.
    token: mio::Token,

    /// TLS session for this connection.
    tls_session: rustls::ServerSession,

    /// The status of the connection.
    state: KeServerConnState,

    /// Logger.
    logger: slog::Logger,
}

impl KeServerConn {
    pub fn new(
        tcp_stream: TcpStream,
        token: mio::Token,
        listener: &KeServerListener,
    ) -> KeServerConn {
        let server_state = listener.state();

        // Create a TLS session from a server-wide configuration.
        let tls_session = rustls::ServerSession::new(&server_state.tls_server_config);
        // Create a child logger for the connection.
        let logger = listener.logger().new(slog::o!("client" => listener.addr().to_string()));

        KeServerConn {
            // Create an `Arc` reference.
            server_state: server_state.clone(),
            tcp_stream,
            tls_session,
            token,
            logger,
            state: KeServerConnState::Connected,
        }
    }

    /// The handler when the connection is ready to ready or write.
    pub fn ready(&mut self, poll: &mut mio::Poll, event: &mio::Event) {
        if event.readiness().is_readable() {
            self.read_ready();
        }

        if event.readiness().is_writable() {
            self.do_tls_write_and_handle_error();
        }

        if !self.is_closed() {
            // TODO: Fix unwrap later.
            self.reregister(poll).unwrap();
        }
    }

    fn read_ready(&mut self) {
        // If this is the first time that `read_ready` is called, it means that we start reading
        // some TLS client hello from the client. So we need to change the state to TlsHandshaking.
        if self.state == KeServerConnState::Connected {
            self.state = KeServerConnState::TlsHandshaking;
        }

        // Read some data from the stream and feed it to the TLS stream.
        let result = self.tls_session.read_tls(&mut self.tcp_stream);

        let read_count = match result {
            Ok(value) => value,
            Err(error) => {
                // If it's a WouldBlock, it's not actually an error. So we don't need to close the
                // connection and return silently.
                if let std::io::ErrorKind::WouldBlock = error.kind() {
                    return;
                }

                // Close the connection on error.
                error!(self.logger, "read error {}", error);
                self.shutdown();
                return;
            }
        };

        // If we reach the end-of-file, just close the connection.
        if read_count == 0 {
            info!(self.logger, "eof");
            self.shutdown();
            return;
        }

        // Process newly received TLS messages.
        let processed = self.tls_session.process_new_packets();

        if let Err(error) = processed {
            error!(self.logger, "cannot process packet: {}", error);
            self.shutdown();
        }

        let mut buf = Vec::new();
        let result = self.tls_session.read_to_end(&mut buf);

        if let Err(error) = result {
            error!(self.logger, "read failed: {}", error);
            self.shutdown();
            return;
        }

        if !buf.is_empty() {
            debug!(self.logger, "plaintext read {},", buf.len());

            // The plaintext is not empty. It means that the handshake is also done. We can change
            // the state now.
            if self.state == KeServerConnState::TlsHandshaking {
                self.state = KeServerConnState::Opened;
            }

            let keys = gen_key(&self.tls_session).unwrap();

            // We have to make sure that the response is not sent yet.
            if self.state == KeServerConnState::Opened {
                // TODO: Fix unwrap later.
                self.tls_session
                    .write_all(&response(keys,
                                         &self.server_state.rotator,
                                         &self.server_state.config.next_port)).unwrap();
                // Mark that the reponse is sent.
                self.state = KeServerConnState::ResponseSent;
            }
        }
    }

    pub fn tls_write(&mut self) -> std::io::Result<usize> {
        self.tls_session.write_tls(&mut self.tcp_stream)
    }

    pub fn do_tls_write_and_handle_error(&mut self) {
        let rc = self.tls_write();
        if rc.is_err() {
            error!(self.logger, "write failed {:?}", rc);
            self.shutdown();
            return;
        }
    }

    /// Register the connection with Poll.
    pub fn register(&self, poll: &mut mio::Poll) -> Result<(), std::io::Error> {
        poll.register(
            &self.tcp_stream,
            self.token,
            self.event_set(),
            mio::PollOpt::level(),
        )
    }

    /// Re-register the connection with Poll.
    pub fn reregister(&self, poll: &mut mio::Poll) -> Result<(), std::io::Error> {
        poll.reregister(
            &self.tcp_stream,
            self.token,
            self.event_set(),
            mio::PollOpt::level(),
        )
    }

    pub fn event_set(&self) -> mio::Ready {
        let rd = self.tls_session.wants_read();
        let wr = self.tls_session.wants_write();

        if rd && wr {
            mio::Ready::readable() | mio::Ready::writable()
        } else if wr {
            mio::Ready::writable()
        } else {
            mio::Ready::readable()
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state == KeServerConnState::Closed
    }

    pub fn shutdown(&mut self) {
        // TODO: Fix unwrap later.
        self.tcp_stream.shutdown(Shutdown::Both).unwrap();
        self.state = KeServerConnState::Closed;
    }
}
