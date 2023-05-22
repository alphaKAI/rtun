use log::{info, trace};
use rtun_common::{
    ApplicationServerManifest, RtunCommunication, RtunServerNegReq, RtunServerNegRes,
    SerializedDataContainer, ServerRequest, SessionID, RTUN_COM_TIMEOUT, SERVER_PORT, PROTOCOL_VER,
};
use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, PartialEq, Eq)]
enum ConnectionBreakReason {
    SrcIsClosed,
    RtunServerTimeout(SessionID),
    RtunServerUnreachable,
}

fn connect(
    session_id: Option<SessionID>,
    mut src: &TcpStream,
    manifest: &ApplicationServerManifest,
) -> ConnectionBreakReason {
    let server = TcpStream::connect(format!("0.0.0.0:{}", SERVER_PORT - 1));
    let want_to_resume = session_id.is_some();

    if server.is_err() {
        info!("connecing failed. please retry...");
        return ConnectionBreakReason::RtunServerUnreachable;
    }

    let mut server = server.unwrap();

    if let Some(session_id) = session_id {
        server
            .write_all(
                &SerializedDataContainer::from_serializable_data(
                    &RtunServerNegReq::ResumeConnection(PROTOCOL_VER, session_id),
                )
                .unwrap()
                .to_one_vec(),
            )
            .unwrap();
    } else {
        server
            .write_all(
                &SerializedDataContainer::from_serializable_data(&RtunServerNegReq::NewConnection(PROTOCOL_VER))
                    .unwrap()
                    .to_one_vec(),
            )
            .unwrap();
    }

    let sdc = SerializedDataContainer::from_reader(&mut server).unwrap();
    match sdc
        .to_serializable_data::<RtunServerNegRes>()
        .expect("server must send RtunServerNegRes at first.")
    {
        RtunServerNegRes::Established(session_id) => {
            info!("Sucessfully Established connection to Server!");

            let active = Arc::new(Mutex::new(true));

            let _heartbeat_sender = {
                let mut server = server.try_clone().unwrap();
                let session_id = session_id;
                let active = active.clone();
                thread::spawn(move || loop {
                    {
                        trace!("Heartbeat sender for {} send heartbeat.", session_id);
                        if server
                            .write_all(
                                &SerializedDataContainer::from_serializable_data(
                                    &RtunCommunication::Heartbeat(session_id),
                                )
                                .unwrap()
                                .to_one_vec(),
                            )
                            .is_err()
                        {
                            trace!("failed to send heartbeat!!!");
                            *active.lock().unwrap() = false;
                            return;
                        }
                        trace!(
                            " - [ok] Heartbeat sender for {} send heartbeat.",
                            session_id
                        );
                    }

                    thread::sleep(RTUN_COM_TIMEOUT / 4)
                })
            };

            let _packet_sender = {
                let mut server = server.try_clone().unwrap();
                let session_id = session_id;
                let manifest = manifest.clone();
                let mut src = src.try_clone().unwrap();
                thread::spawn(move || {
                    if !want_to_resume {
                        info!("register manifest");
                        server
                            .write_all(
                                &SerializedDataContainer::from_serializable_data(
                                    &RtunCommunication::ServerRequest(
                                        session_id,
                                        rtun_common::ServerRequest::RegisterManifest(manifest),
                                    ),
                                )
                                .unwrap()
                                .to_one_vec(),
                            )
                            .expect("failed to send manifest");
                    }

                    let mut buf: [u8; 1024] = [0; 1024];
                    loop {
                        {
                            let n = src.read(&mut buf).unwrap();
                            info!("Read data from SrcApplication, {n} bytes");
                            if n == 0 {
                                info!("Src is Closed!!");
                                server
                                    .write_all(
                                        &SerializedDataContainer::from_serializable_data(
                                            &RtunCommunication::ServerRequest(
                                                session_id,
                                                ServerRequest::SrcApplicationIsClosed,
                                            ),
                                        )
                                        .unwrap()
                                        .to_one_vec(),
                                    )
                                    .expect("failed to send manifest");
                                return ConnectionBreakReason::SrcIsClosed;
                            }
                            server
                                .write_all(
                                    &SerializedDataContainer::from_serializable_data(
                                        &RtunCommunication::Packet(session_id, buf[..n].to_vec()),
                                    )
                                    .unwrap()
                                    .to_one_vec(),
                                )
                                .expect("failed to send Heartbeat");
                        }
                    }
                })
            };

            let mut last_hb_recv = None;
            server
                .set_read_timeout(Some(std::time::Duration::from_millis(50)))
                .expect("failed to set read timeout");
            loop {
                let sdc = SerializedDataContainer::from_reader(&mut server);

                let is_active = { *active.lock().unwrap() };
                if is_active {
                    if let Ok(sdc) = sdc {
                        match sdc
                            .to_serializable_data::<RtunCommunication>()
                            .expect("client must send RtunServerNegReq at first.")
                        {
                            RtunCommunication::Heartbeat(server_session_id) => {
                                if server_session_id == session_id {
                                    info!("Received valid heartbeat for {}", session_id);
                                    info!("session of {:?} is still active!", session_id);
                                    last_hb_recv = Some(chrono::Local::now());
                                }
                            }
                            RtunCommunication::Packet(server_session_id, payload) => {
                                info!("RtunCommunication::Packet({server_session_id} - {session_id}, {} bytes)", payload.len());
                                if server_session_id == session_id {
                                    info!("Received packet from server for {}", session_id);

                                    src.write_all(&payload).unwrap();
                                }
                            }
                            RtunCommunication::InvalidRequest(reason) => {
                                panic!("You must handle this error: {reason:?}")
                            }
                            RtunCommunication::ClientRequest(server_session_id, req) => {
                                if server_session_id == session_id {
                                    match req {
                                        rtun_common::ClientRequest::SrcApplicationIsClosed => {
                                            info!("Received ClientRequest::SrcApplicationIsClosed");
                                            return ConnectionBreakReason::SrcIsClosed;
                                        }
                                    }
                                }
                            }
                            // Clint need not handle these req
                            RtunCommunication::ServerRequest(_, _) => (),
                        }
                    } else if let Some(last_hb) = last_hb_recv {
                        if (chrono::Local::now() - last_hb).to_std().unwrap() >= RTUN_COM_TIMEOUT {
                            info!("connection timeout.");

                            *active.lock().unwrap() = false;

                            return ConnectionBreakReason::RtunServerTimeout(session_id);
                        }
                    }
                } else {
                    return ConnectionBreakReason::RtunServerTimeout(session_id);
                }
            }
        }
        RtunServerNegRes::InvalidSessionID => {
            panic!("InvalidSessionID, failed to resume");
        }
        RtunServerNegRes::ResumeFailed => {
            panic!("failed to resume");
        }
        RtunServerNegRes::ProtocolVersionMismatched => {
            panic!("Protocol Version Mismatched between this client and the server.");
        }
    }
}

fn main() -> std::io::Result<()> {
    env::set_var("RUST_LOG", "trace");
    env_logger::init();

    let src_port: u16 = 4000;
    loop {
        let listener = TcpListener::bind(format!("0.0.0.0:{src_port}")).unwrap();
        info!("Start listening for application connect at 0.0.0.0:{src_port}");

        let (src, addr) = listener.accept().unwrap();
        info!("Accept a request from an application with {addr:?}");
        let manifest = ApplicationServerManifest {
            ipaddr: "127.0.0.1".to_owned(),
            port: 5000,
        };

        let mut session_id = None;
        loop {
            match connect(session_id, &src, &manifest) {
                ConnectionBreakReason::SrcIsClosed => info!("ConnectionBreakReason::SrcIsClosed"),
                ConnectionBreakReason::RtunServerTimeout(sever_session_id) => {
                    session_id = Some(sever_session_id)
                }
                ConnectionBreakReason::RtunServerUnreachable => {
                    info!("ConnectionBreakReason::RtunServerUnreachable")
                }
            }
            info!("reconnecting...");
        }
    }
}
