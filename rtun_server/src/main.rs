use log::{info, trace};
use rtun_common::{
    new_session_id, ClientInfo, ClientRequest, InvalidReason, RtunCommunication, RtunServerNegReq,
    RtunServerNegRes, RtunState, SerializedDataContainer, ServerRequest, SessionID, PROTOCOL_VER,
    RTUN_COM_TIMEOUT, SERVER_PORT,
};
use std::collections::HashMap;
use std::io::prelude::*;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::{env, thread};

pub type SessionStore = HashMap<SessionID, ClientInfo>;

fn handle_client(mut client: TcpStream, session_store: Arc<Mutex<SessionStore>>) {
    let sdc = SerializedDataContainer::from_reader(&mut client).unwrap();

    let mut app_stream: Option<TcpStream> = None;

    let (session_id, client_info) = match sdc
        .to_serializable_data::<RtunServerNegReq>()
        .expect("client must send RtunServerNegReq at first.")
    {
        RtunServerNegReq::NewConnection(protocol_ver) => {
            if protocol_ver != PROTOCOL_VER {
                info!("The protocol version of the clinet is not matched that of this server.");
                client
                    .write_all(
                        &SerializedDataContainer::from_serializable_data(
                            &RtunServerNegRes::ProtocolVersionMismatched,
                        )
                        .unwrap()
                        .to_one_vec(),
                    )
                    .unwrap();

                return;
            }

            trace!("- [1] lock request");
            let mut session_store = session_store.lock().expect("failed to lock session_store");
            trace!("- [1] lock aquire");
            let session_id = new_session_id(&session_store.keys().collect());
            let client_info = ClientInfo::new(session_id, RtunState::Established);
            session_store.insert(session_id, client_info.clone());

            client
                .write_all(
                    &SerializedDataContainer::from_serializable_data(
                        &RtunServerNegRes::Established(session_id),
                    )
                    .unwrap()
                    .to_one_vec(),
                )
                .unwrap();

            info!("Connection Established! SessionID: {}", session_id);

            trace!("- [1] lock release");
            (session_id, client_info)
        }
        RtunServerNegReq::ResumeConnection(protocol_ver, session_id) => {
            if protocol_ver != PROTOCOL_VER {
                client
                    .write_all(
                        &SerializedDataContainer::from_serializable_data(
                            &RtunServerNegRes::ProtocolVersionMismatched,
                        )
                        .unwrap()
                        .to_one_vec(),
                    )
                    .unwrap();

                return;
            }
            trace!("- [2] lock request");
            let mut session_store = session_store.lock().expect("failed to lock session_store");
            trace!("- [2] lock aquire");

            if session_store.contains_key(&session_id) {
                // check state of session
                let client_info = session_store.get_mut(&session_id).unwrap();

                if client_info.check_resumable(&session_id) {
                    // ok
                    info!("Session({}) : start resuming", session_id);
                    client
                        .write_all(
                            &SerializedDataContainer::from_serializable_data(
                                &RtunServerNegRes::Established(session_id),
                            )
                            .unwrap()
                            .to_one_vec(),
                        )
                        .unwrap();

                    client_info.resume();
                    app_stream = client_info
                        .application_stream
                        .as_ref()
                        .map(|stream| stream.try_clone().unwrap());

                    info!("Connection of Session({}) resumed!", session_id);
                    trace!("- [2] lock release");
                    (session_id, client_info.clone())
                } else {
                    client
                        .write_all(
                            &SerializedDataContainer::from_serializable_data(
                                &RtunServerNegRes::ResumeFailed,
                            )
                            .unwrap()
                            .to_one_vec(),
                        )
                        .unwrap();

                    info!("Session({}) resume failed", session_id);
                    return;
                }
            } else {
                // error
                client
                    .write_all(
                        &SerializedDataContainer::from_serializable_data(
                            &RtunServerNegRes::InvalidSessionID,
                        )
                        .unwrap()
                        .to_one_vec(),
                    )
                    .unwrap();

                info!("Error: Invalid Session ID given.");
                return;
            }
        }
    };

    enum HB {
        Exit,
    }

    let (hb_tx, hb_rx) = std::sync::mpsc::channel();

    let _heartbeat_sender = {
        let mut client = client.try_clone().unwrap();
        let session_id = session_id;
        thread::spawn(move || loop {
            {
                let cont = match hb_rx.try_recv() {
                    Ok(hb) => match hb {
                        HB::Exit => false,
                    },
                    Err(_) => true,
                };
                if cont {
                    trace!("Heartbeat sender for {} send heartbeat.", session_id);
                    client
                        .write_all(
                            &SerializedDataContainer::from_serializable_data(
                                &RtunCommunication::Heartbeat(session_id),
                            )
                            .unwrap()
                            .to_one_vec(),
                        )
                        .expect("failed to send Heartbeat");
                } else {
                    trace!("Heartbeat sender for {} exit.", session_id);
                    return;
                }
                trace!("- [3] lock release");
            }

            thread::sleep(RTUN_COM_TIMEOUT / 4)
        })
    };

    enum AR {
        Stream(TcpStream),
    }
    let (ar_tx, ar_rx) = std::sync::mpsc::channel();

    let _application_reader = {
        let mut client = client.try_clone().unwrap();
        thread::spawn(move || {
            #[allow(unused_assignments)]
            let mut app_stream = None;

            loop {
                match ar_rx.try_recv() {
                    Ok(ar) => {
                        let AR::Stream(stream) = ar;
                        app_stream = Some(stream);
                        break;
                    }
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }

            let mut buf: [u8; 1024] = [0; 1024];
            loop {
                {
                    match app_stream.as_ref().unwrap().read(&mut buf) {
                        Ok(n) => {
                            info!("Read data from Application, {n} bytes");
                            if n == 0 {
                                info!("client is closed");
                                return;
                            }

                            client
                                    .write_all(
                                        &SerializedDataContainer::from_serializable_data(
                                            &RtunCommunication::Packet(session_id,
                                                buf[..n].to_vec()
                                            ),
                                        )
                                        .unwrap()
                                        .to_one_vec(),
                                    )
                                    .expect("failed to send InvalidRequest(InvalidReason::CanNotChangeManifest)");
                        }
                        Err(_) => {
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    trace!("- [5] lock release");
                }
            }
        })
    };

    client
        .set_read_timeout(Some(RTUN_COM_TIMEOUT))
        .expect("failed to set read timeout");
    loop {
        let sdc = SerializedDataContainer::from_reader(&mut client);

        if let Ok(sdc) = sdc {
            match sdc
                .to_serializable_data::<RtunCommunication>()
                .expect("client must send RtunServerNegReq at first.")
            {
                RtunCommunication::Heartbeat(client_session_id) => {
                    if client_session_id == session_id {
                        info!("Received valid heartbeat for {}", session_id);
                        info!(
                            "session of {:?} is still active!",
                            (session_id, &client_info)
                        );
                    }
                }
                RtunCommunication::Packet(client_session_id, payload) => {
                    if client_session_id == session_id {
                        info!("Receive RtunCommunication::Packet");
                        trace!("- [6] lock request");
                        let mut session_store = session_store.lock().unwrap();
                        trace!("- [6] lock aquire");
                        let mut client_info = session_store.get_mut(&session_id).unwrap();

                        if client_info.application_stream.is_none() {
                            if let Some(manifest) = client_info.manifest.as_ref() {
                                match TcpStream::connect(format!(
                                    "{}:{}",
                                    manifest.ipaddr, manifest.port
                                )) {
                                    Ok(mut stream) => {
                                        client_info.application_stream = {
                                            app_stream = Some(stream.try_clone().unwrap());
                                            ar_tx
                                                .send(AR::Stream(stream.try_clone().unwrap()))
                                                .unwrap();

                                            stream
                                                .write_all(&payload)
                                                .expect("failed to write payload to application.");

                                            Some(stream)
                                        }
                                    }
                                    Err(_) => {
                                        client
                                            .write_all(
                                                &SerializedDataContainer::from_serializable_data(
                                                    &RtunCommunication::InvalidRequest(InvalidReason::FailedToConnectApplicationServer),
                                                )
                                                .unwrap()
                                                .to_one_vec(),
                                            )
                                            .expect("failed to send InvalidRequest(InvalidReason::FailedToConnectApplicationServer)");
                                    }
                                }
                            } else {
                                client
                                    .write_all(
                                        &SerializedDataContainer::from_serializable_data(
                                            &RtunCommunication::InvalidRequest(
                                                InvalidReason::PleaseRegisterManifestAtFirst,
                                            ),
                                        )
                                        .unwrap()
                                        .to_one_vec(),
                                    )
                                    .expect("failed to send InvalidRequest(InvalidReason::PleaseRegisterManifestAtFirst)");
                            }
                        } else {
                            app_stream
                                .as_ref()
                                .unwrap()
                                .write_all(&payload)
                                .expect("failed to write payload to application.");
                        }
                        trace!("- [6] lock release");
                    }
                }
                RtunCommunication::ServerRequest(client_session_id, req) => {
                    if client_session_id == session_id {
                        match req {
                            ServerRequest::RegisterManifest(manifest) => {
                                trace!("- [7] lock request");
                                let mut session_store = session_store.lock().unwrap();
                                trace!("- [7] lock aquire");
                                let mut client_info = session_store.get_mut(&session_id).unwrap();
                                if client_info.manifest.is_none() {
                                    client_info.manifest = Some(manifest);
                                } else {
                                    client
                                    .write_all(
                                        &SerializedDataContainer::from_serializable_data(
                                            &RtunCommunication::InvalidRequest(
                                                InvalidReason::CanNotChangeManifest,
                                            ),
                                        )
                                        .unwrap()
                                        .to_one_vec(),
                                    )
                                    .expect("failed to send InvalidRequest(InvalidReason::CanNotChangeManifest)");
                                }
                                trace!("- [7] lock release");
                            }
                            ServerRequest::SrcApplicationIsClosed => {
                                // close
                                info!("Received ServerRequest::SrcApplicationIsClosed");
                                client
                                    .write_all(
                                        &SerializedDataContainer::from_serializable_data(
                                            &RtunCommunication::ClientRequest(
                                                session_id,
                                                ClientRequest::SrcApplicationIsClosed,
                                            ),
                                        )
                                        .unwrap()
                                        .to_one_vec(),
                                    )
                                    .expect("failed to send InvalidRequest(InvalidReason::CanNotChangeManifest)");

                                trace!("- [8] lock request");
                                let mut session_store = session_store.lock().unwrap();
                                trace!("- [8] lock aquire");
                                let client_info = session_store.get_mut(&session_id).unwrap();
                                client_info.close();
                                hb_tx.send(HB::Exit).unwrap();
                                trace!("- [8] lock release");
                            }
                        }
                    }
                }
                RtunCommunication::InvalidRequest(_) => (),
                RtunCommunication::ClientRequest(_, _) => (),
            }
        } else {
            info!("connection timeout. going to suspend...");
            // connection lost. suspend.
            trace!("- [8] lock request");
            let mut session_store = session_store.lock().unwrap();
            trace!("- [8] lock aquire");
            let client_info = session_store.get_mut(&session_id).unwrap();
            client_info.suspend();
            hb_tx.send(HB::Exit).unwrap();

            trace!("- [8] lock release");
            return;
        }
    }
}

// TODO, suspended connections.

fn main() -> std::io::Result<()> {
    env::set_var("RUST_LOG", "trace");
    env_logger::init();

    let server = TcpListener::bind(format!("0.0.0.0:{SERVER_PORT}"))?;

    let session_store = Arc::new(Mutex::new(SessionStore::new()));

    info!("Server started: 0.0.0.0:{SERVER_PORT}");
    info!("waing for connect...");
    for client in server.incoming() {
        let client = client?;
        let session_store = session_store.clone();

        info!("New client connected!");
        thread::spawn(move || {
            handle_client(client, session_store);
            info!("waing for connect...");
        });
    }

    Ok(())
}
