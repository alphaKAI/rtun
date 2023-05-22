use rand::prelude::*;
use rmp_serde::{self, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::net::TcpStream;
use std::time::Duration;
use std::{convert::TryInto, mem::size_of};

pub const SERVER_PORT: u16 = 32540;
pub type ProtocolVer = u64;
pub const PROTOCOL_VER: ProtocolVer = 0x0000_0000_0000_0001;
pub const RTUN_NEG_TIMEOUT: Duration = std::time::Duration::from_millis(1000);
pub const RTUN_COM_TIMEOUT: Duration = std::time::Duration::from_millis(6000);

#[derive(Debug, Serialize, Deserialize)]
pub enum RtunServerNegReq {
    NewConnection(ProtocolVer),
    ResumeConnection(ProtocolVer, SessionID),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RtunServerNegRes {
    Established(SessionID),
    InvalidSessionID,
    ResumeFailed,
    ProtocolVersionMismatched
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RtunCommunication {
    Heartbeat(SessionID),
    Packet(SessionID, Vec<u8>),
    ServerRequest(SessionID, ServerRequest),
    ClientRequest(SessionID, ClientRequest),
    InvalidRequest(InvalidReason)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum InvalidReason {
    ManifestAlreadyConfigured,
    PleaseRegisterManifestAtFirst,
    FailedToConnectApplicationServer,
    CanNotChangeManifest
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerRequest {
    RegisterManifest(ApplicationServerManifest),
    SrcApplicationIsClosed,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientRequest {
    SrcApplicationIsClosed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtunState {
    NotEstablished,
    Established,
    Closed,
    Suspended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplicationServerManifest {
    pub ipaddr: String,
    pub port: u16,
}

#[derive(Debug)]
pub struct ClientInfo {
    pub session_id: SessionID,
    pub state: RtunState,
    pub application_stream: Option<TcpStream>,
    pub manifest: Option<ApplicationServerManifest>,
}

impl Clone for ClientInfo {
    fn clone(&self) -> Self {
        let current_stream = self
            .application_stream
            .as_ref()
            .map(|stream| stream.try_clone().expect("failed to clone current_stream"));

        Self {
            session_id: self.session_id,
            state: self.state.clone(),
            application_stream: current_stream,
            manifest: self.manifest.clone(),
        }
    }
}

impl ClientInfo {
    pub fn new(session_id: SessionID, state: RtunState) -> Self {
        Self {
            session_id,
            state,
            application_stream: None,
            manifest: None,
        }
    }

    pub fn check_resumable(&self, session_id: &SessionID) -> bool {
        // pre cond... session_id is the same and state is Suspended.
        self.session_id == *session_id && self.state == RtunState::Suspended
    }

    pub fn suspend(&mut self) {
        if self.state == RtunState::Established {
            self.state = RtunState::Suspended
        } else {
            panic!("Suspend operation can be permitted for Established only but current state is {:?}", self.state)
        }
    }

    pub fn resume(&mut self) {
        if self.state == RtunState::Suspended {
            self.state = RtunState::Established
        } else {
            panic!("Resume operation can be permitted for Suspended only but current state is {:?}", self.state)
        }
    }

    pub fn close(&mut self) {
        self.state = RtunState::Closed;
        if let Some(stream) = self.application_stream.as_ref() {
            stream.shutdown(std::net::Shutdown::Both).expect("failed to close stream");
        }
    }
}

pub type SessionID = u64;

pub fn new_session_id(session_ids: &HashSet<&SessionID>) -> SessionID {
    let mut rng = rand::thread_rng();

    loop {
        let new_id = rng.next_u64();

        // if conflict, regenerate session id.
        if !session_ids.contains(&new_id) {
            return new_id;
        }
    }
}

#[derive(Debug)]
pub struct SerializedDataContainer {
    size: usize,
    data: Vec<u8>,
}

impl SerializedDataContainer {
    pub fn new(v: &[u8]) -> Self {
        Self {
            size: v.len(),
            data: v.to_owned(),
        }
    }

    pub fn to_one_vec(&self) -> Vec<u8> {
        let mut ret = vec![];

        ret.append(&mut self.size.to_le_bytes().to_vec());
        ret.append(&mut self.data.clone());

        ret
    }

    pub fn from_reader<T>(reader: &mut T) -> Result<Self, std::io::Error>
    where
        T: Read,
    {
        let mut size_buffer = [0; size_of::<usize>()];
        reader.read_exact(&mut size_buffer).and_then(|_| {
            let size = usize::from_le_bytes(size_buffer);
            let mut data = vec![];

            reader.take(size as u64).read_to_end(&mut data)?;

            Ok(Self { size, data })
        })
    }

    pub fn from_one_vec(v: Vec<u8>) -> Option<Self> {
        if v.len() >= size_of::<usize>() {
            let size = usize::from_le_bytes(
                v[0..size_of::<usize>()]
                    .try_into()
                    .expect("Failed to parse size of the data container"),
            );
            let data = v[size_of::<usize>()..size_of::<usize>() + size]
                .try_into()
                .expect("Failed to get data of the data container");

            Some(Self { size, data })
        } else {
            None
        }
    }

    pub fn from_serializable_data<T>(t: &T) -> Option<Self>
    where
        T: Serialize,
    {
        let mut data = vec![];
        t.serialize(&mut Serializer::new(&mut data)).ok().map(|_| {
            let size = data.len();
            Self { size, data }
        })
    }

    pub fn to_serializable_data<T: for<'de> Deserialize<'de>>(&self) -> Option<T> {
        rmp_serde::from_slice(&self.data).ok()
    }
}
