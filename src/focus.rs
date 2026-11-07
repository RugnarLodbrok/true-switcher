use crate::input_events::{InputEventMessage, InputEventSender};
use std::io;

use zbus::blocking::Connection;

const BUS_NAME: &str = "org.true_switcher";
const OBJECT_PATH: &str = "/org/true_switcher";

pub struct FocusService {
    _connection: Connection,
}

impl FocusService {
    pub fn start(sender: InputEventSender) -> io::Result<Self> {
        let connection = Connection::session().map_err(io::Error::other)?;
        connection
            .request_name(BUS_NAME)
            .map_err(io::Error::other)?;
        connection
            .object_server()
            .at(OBJECT_PATH, FocusChanged { sender })
            .map_err(io::Error::other)?;

        Ok(Self {
            _connection: connection,
        })
    }
}

struct FocusChanged {
    sender: InputEventSender,
}

#[zbus::interface(name = "org.true_switcher.Focus")]
impl FocusChanged {
    #[zbus(name = "FocusChanged")]
    fn focus_changed(&self) {
        self.sender.send(InputEventMessage::FocusChanged);
    }
}
